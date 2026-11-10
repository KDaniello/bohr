//! The plan that stages a stored finding on a plain Linux host, and the script that carries it out.
//!
//! The script starts the recorded programs as ordinary processes and delivers the recorded node
//! faults with signals: `kill -9` for a kill, `SIGSTOP` and `SIGCONT` for a freeze. It restarts a
//! node where the run restarted it, repeats the whole run, and counts how often the recorded exit
//! status comes back.
//!
//! A killed process keeps its unsynced writes in the page cache; a crash on the model loses them.
//! For a run with a kill, the script gives each node a dm-flakey device of its own, drops that
//! node's writes at its kill and remounts, so only synced data survives, on that node alone.
//!
//! Virtual time differs from real time. Each repetition draws a fault's moment between zero and
//! twice the recorded one, and the rate covers that window.
//!
//! The script does not stage partitions, lost or delayed packets, disk errors and tears, clock
//! faults or probe verdicts. It lists the ones the run had and judges exit statuses only.

use std::fmt::Write as _;

use bohr_report::RunModel;
use bohr_trace::{EventKind, ExitStatus, FaultKind, FaultTarget, NodeState, PacketFate};

/// A node's argument vector and environment, as the scenario gave them.
pub type Node = (Vec<String>, Vec<(String, String)>);

/// Refuses a working directory a node's root could be left by: it is absolute and climbs nowhere.
fn check_cwd(cwd: &str) -> Result<(), String> {
    let climbs = cwd.split('/').any(|part| part == "..");
    if !cwd.starts_with('/') || climbs {
        return Err(format!("the working directory {cwd:?} is not an absolute path inside a root"));
    }
    Ok(())
}

/// Refuses an environment name no shell variable has: `env` would read one that begins with `-`
/// as an option of its own.
fn check_env_key(key: &str) -> Result<(), String> {
    let mut chars = key.chars();
    let first = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !first || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("the environment names {key:?}, which is no variable's name"));
    }
    Ok(())
}

/// What the script stages.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// Each node's argument vector and environment.
    pub nodes: Vec<Node>,
    /// The directory every node's filesystem started from, where the scenario prepared one.
    pub state: Option<String>,
    /// The working directory every node starts in.
    pub cwd: String,
    /// The faults, in order of virtual time.
    pub actions: Vec<Action>,
    /// How long the run was, in nanoseconds of virtual time.
    pub horizon_ns: u64,
    /// How long the run went on after its last fault, in nanoseconds of virtual time.
    pub rest_ns: u64,
    /// The exit statuses that make the finding, as `(node, status)`; 128 + n for signal n.
    pub expect: Vec<(u32, i32)>,
    /// Faults the run delivered that the script cannot stage.
    pub unstaged: Vec<String>,
}

/// One fault the script stages: when, on which node, and what.
#[derive(Debug, PartialEq, Eq)]
pub struct Action {
    /// When, in nanoseconds of virtual time.
    pub at_ns: u64,
    /// On which node.
    pub node: u32,
    /// What: `kill`, `freeze`, `thaw`, `restart`.
    pub what: &'static str,
}

impl Plan {
    /// The plan for a stored run, or why it has none.
    ///
    /// Whether the programs it names are, byte for byte, the ones the run recorded is a reading
    /// of the host, which the caller makes.
    ///
    /// # Errors
    ///
    /// The run names no program a host could start, or nothing a host could judge.
    pub fn of(model: &RunModel) -> Result<Self, String> {
        let loaded = bohr_world::config::parse(&model.config)
            .map_err(|e| format!("the run's configuration does not parse: {e}"))?;
        let cluster = loaded.cluster.as_ref();
        let count = cluster.map_or(1, |c| c.nodes.max(1));
        // A program given on the command line is in the bundle's record of what it started.
        let given: Vec<String> = model
            .binaries
            .iter()
            .find(|b| b.role == bohr_trace::BinaryRole::Executable)
            .map(|first| std::iter::once(first.path.clone()).chain(first.args.clone()).collect())
            .unwrap_or_default();
        let env: Vec<(String, String)> = cluster.map(|c| c.env.clone()).unwrap_or_default();
        let mut nodes = Vec::new();
        for index in 0..count {
            let argv: Vec<String> = match cluster {
                Some(c) if !c.argv.is_empty() || !c.programs.is_empty() => c.argv_for(index),
                _ => given.iter().map(|word| word.replace("{node}", &index.to_string())).collect(),
            };
            if argv.is_empty() || argv[0].starts_with("builtin:") {
                return Err("the run names no program for a node; a run of a built-in guest has \
                            nothing to start on a host"
                    .to_owned());
            }
            nodes.push((argv, env.clone()));
        }
        let state = loaded.state_source.as_ref().map(|source| source.dir.clone());
        let cwd = loaded.cwd.clone().unwrap_or_else(|| "/".to_owned());
        check_cwd(&cwd)?;
        for (_, env) in &nodes {
            for (key, _) in env {
                check_env_key(key)?;
            }
        }

        let (actions, unstaged, expect) = read_run(&model.events);
        if expect.is_empty() {
            return Err("the run records no process that failed, and an exit status is the only \
                        judgement a host without bohr can make"
                .to_owned());
        }
        let horizon_ns = model.manifest.counters.ticks;
        let last = actions.last().map_or(0, |a| a.at_ns);
        Ok(Self {
            nodes,
            state,
            cwd,
            actions,
            horizon_ns,
            rest_ns: horizon_ns.saturating_sub(last),
            expect: expect.into_iter().collect(),
            unstaged: unstaged.into_iter().collect(),
        })
    }

    /// Whether the run crashed a node, which the script stages with dm-flakey, as root.
    #[must_use]
    pub fn crashes(&self) -> bool {
        self.actions.iter().any(|a| a.what == "kill")
    }
}

/// `text` as a single-quoted shell word.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The script for `plan`; `bundle` is the name it prints.
#[must_use]
pub fn script(plan: &Plan, bundle: &str, runs: u32) -> String {
    let mut out = String::new();
    // A name can carry a newline, which would end the comment and begin a command.
    let bundle = bundle.replace(['\n', '\r'], " ");
    let commands: String = plan
        .nodes
        .iter()
        .enumerate()
        .map(|(index, (argv, _))| {
            let words: Vec<String> = argv.iter().map(|word| quote(word)).collect();
            format!("#   node {index}: {}\n", words.join(" "))
        })
        .collect();
    let _ = writeln!(
        out,
        "#!/usr/bin/env bash\n\
         # Written by `bohr confirm {bundle}`. Stages the recorded run on this host without bohr.\n\
         #\n\
         # It runs, as root, the commands the run recorded. A bundle from someone else is code from\n\
         # someone else: read these before running it.\n\
         {commands}\
         #\n\
         # Run it from the directory the run started in, so the program paths resolve.\n\
         # Each repetition starts every node fresh, delivers the recorded faults in order, and checks\n\
         # for the recorded exit status. Each gap between faults is drawn between zero and twice the\n\
         # recorded one, and at least up to SPREAD_NS: virtual time counts syscalls and sleeps, not\n\
         # computation, so an early moment of the run comes later on a real host.\n\
         # RUNS and SPREAD_NS can be set from the environment. The last line prints the rate.\n\
         #\n\
         # Faults the run had that this script does not stage: {}.\n\
         set -u\n\
         RUNS=${{RUNS:-{runs}}}\n\
         SPREAD_NS=${{SPREAD_NS:-2000000000}}\n\
         HORIZON_NS={}\n\
         REST_NS={}\n\
         NODES={}\n\
         CWD={}\n\
         BASE=$(mktemp -d)\n\
         # POWER_LOSS=0 stages a crash as a plain kill, which tells a process-crash defect from a\n\
         # power-loss one.\n\
         POWER_LOSS=${{POWER_LOSS:-{}}}",
        if plan.unstaged.is_empty() { "nothing".to_owned() } else { plan.unstaged.join(", ") },
        plan.horizon_ns,
        plan.rest_ns,
        plan.nodes.len(),
        quote(&plan.cwd),
        u8::from(plan.crashes()),
    );
    out.push_str(
        r#"
# A crash keeps only synced data. Each node's files sit on a device of its own under $DISK, as each
# node in the world has its own disk; at a node's kill its device drops writes, and it is remounted
# after. Pids, statuses and logs stay off the devices.
#
# A repetition is void, and not counted, when the staging itself fails: a node that outlives its
# kill, an unmount that does not happen, or a filesystem `e2fsck` finds damaged. Each repetition
# starts on fresh filesystems.
DISK="$BASE/disk"; mkdir -p "$DISK"
declare -a LOOP=()
# Each node runs in a root of its own, in a mount namespace of its own with /dev and /proc, so its
# absolute paths land on its own disk as they did in the world. The program has to be static, as
# every program bohr runs is. `unshare --root --wd` enters the root and the working directory.
[ "$(id -u)" = 0 ] || { echo "run as root: each node gets its own root, and a crash needs dm-flakey"; exit 2; }
UNSHARE=$(command -v unshare); MOUNT=$(command -v mount)
"$UNSHARE" --help | grep -q -- --wd || { echo "unshare has no --wd: util-linux 2.38 or later is needed"; exit 2; }
IN_ROOT="$MOUNT --rbind /dev \"\$1/dev\" && $MOUNT -t proc proc \"\$1/proc\" && r=\$1 && w=\$2 && shift 2 && exec $UNSHARE --root=\"\$r\" --wd=\"\$w\" \"\$@\""
enter_root() {
  mkdir -p "$1/.bohr" "$1/dev" "$1/proc" "$1/tmp" "$1$CWD" && cp "$2" "$1/.bohr/program"
}
dm() { echo "bohr-confirm-$$-$1"; }
linear() {
  dmsetup load "$(dm "$1")" --table "0 $SECTORS linear ${LOOP[$1]} 0" &&
    dmsetup suspend "$(dm "$1")" && dmsetup resume "$(dm "$1")"
}
if [ "$POWER_LOSS" = 1 ]; then
  modprobe dm-flakey 2>/dev/null
  for i in $(seq 0 $(( NODES - 1 ))); do
    truncate -s 1G "$BASE.img-$i"
    LOOP[i]=$(losetup -f --show "$BASE.img-$i")
    SECTORS=$(blockdev --getsz "${LOOP[i]}")
    dmsetup create "$(dm "$i")" --table "0 $SECTORS linear ${LOOP[i]} 0"
  done
fi
# A crash cut short by a void leaves its device dropping writes, so each fresh disk is linear first.
fresh_disk() {
  for i in $(seq 0 $(( NODES - 1 ))); do
    mkdir -p "$DISK/node-$i"
    [ "$POWER_LOSS" = 1 ] || continue
    if mountpoint -q "$DISK/node-$i"; then umount "$DISK/node-$i" || return 1; fi
    linear "$i" && mkfs.ext4 -q -F "/dev/mapper/$(dm "$i")" &&
      mount "/dev/mapper/$(dm "$i")" "$DISK/node-$i" && rmdir "$DISK/node-$i/lost+found" || return 1
  done
}
cleanup() {
  for f in "$BASE"/pid-*; do [ -f "$f" ] && kill -9 "$(cat "$f")" 2>/dev/null; done
  if [ "$POWER_LOSS" = 1 ]; then
    sleep 0.2
    for i in "${!LOOP[@]}"; do
      umount "$DISK/node-$i" 2>/dev/null; dmsetup remove "$(dm "$i")" 2>/dev/null
      losetup -d "${LOOP[i]}" 2>/dev/null; rm -f "$BASE.img-$i"
    done
  fi
  for a in "${ADDED[@]}"; do ip addr del "$a/32" dev lo 2>/dev/null; done
  rm -rf "$BASE"
}
declare -a ADDED=()
trap cleanup EXIT
trap 'exit 1' INT TERM

# Each node binds the address the world gave it, 10.66.0.(n+1), so those addresses go on the
# loopback device for the run, and the ones the script added come off at its end.
addr_of() { local h=$(( $1 + 1 )); echo "10.66.$(( h >> 8 )).$(( h & 255 ))"; }
for i in $(seq 0 $(( NODES - 1 ))); do
  a=$(addr_of "$i")
  ip -4 addr show dev lo | grep -q " $a/" && continue
  ip addr add "$a/32" dev lo || { echo "cannot put $a on lo"; exit 2; }
  ADDED+=("$a")
done

# In the recorded run a node is up when a fault reaches it. One that has exited here was not
# staged, and the repetition is void, unless it exited with a status that makes the finding.
up() {
  [ -f "$BASE/pid-$1" ] && kill -0 "$(cat "$BASE/pid-$1")" 2>/dev/null && return 0
  sleep 0.2
  local s; s=$(cat "$BASE/status-$1" 2>/dev/null)
  for e in "${EXPECT[@]}"; do [ "$1:$s" = "$e" ] && { EARLY=1; return 1; }; done
  VOID="node $1 had exited before its fault, with status ${s:-unknown}"
  return 1
}

# Kills the node processes and waits until each is gone, for at most ten seconds.
kill_and_wait() {
  local pids=()
  for f in "$@"; do [ -f "$f" ] && pids+=("$(cat "$f")"); done
  [ "${#pids[@]}" = 0 ] && return 0
  kill -9 "${pids[@]}" 2>/dev/null
  for _ in $(seq 100); do
    alive=0
    for p in "${pids[@]}"; do kill -0 "$p" 2>/dev/null && alive=1; done
    [ "$alive" = 0 ] && return 0
    sleep 0.1
  done
  return 1
}
crash() {
  if [ "$POWER_LOSS" = 1 ]; then
    local d="$DISK/node-$1"
    dmsetup load "$(dm "$1")" --table "0 $SECTORS flakey ${LOOP[$1]} 0 0 3600 1 drop_writes"
    dmsetup suspend --nolockfs "$(dm "$1")"; dmsetup resume "$(dm "$1")"
    kill_and_wait "$BASE/pid-$1" || { VOID="a node outlived its kill"; return; }
    for _ in $(seq 50); do umount "$d" 2>/dev/null && break; sleep 0.1; done
    mountpoint -q "$d" && { VOID="the disk would not unmount"; return; }
    linear "$1"
    # Replays the journal, as mounting after a power cut does, then checks what is left.
    mount "/dev/mapper/$(dm "$1")" "$d" && umount "$d"
    e2fsck -fn "/dev/mapper/$(dm "$1")" >/dev/null 2>&1 || { VOID="e2fsck found the filesystem damaged"; return; }
    mount "/dev/mapper/$(dm "$1")" "$d"
  else
    kill_and_wait "$BASE/pid-$1" || VOID="a node outlived its kill"
  fi
}
"#,
    );
    for (index, (argv, env)) in plan.nodes.iter().enumerate() {
        // The program is copied into the node's root and the rest of the command line is given
        // verbatim: inside its own root the node's absolute paths are its own, as in the world.
        let words: Vec<String> = argv.iter().skip(1).map(|w| quote(w)).collect();
        let vars: Vec<String> = env.iter().map(|(k, v)| quote(&format!("{k}={v}"))).collect();
        // A start of a node still running restarts it, as a restart in the world does. The pid is
        // written by the background shell, and a fault drawn right after the start needs it.
        let _ = writeln!(
            out,
            "start_{index}() {{\n  \
             kill_and_wait \"$BASE/pid-{index}\" || VOID=\"a node outlived its restart\"\n  \
             rm -f \"$BASE/pid-{index}\"\n  \
             ROOT=\"$DISK/node-{index}\"\n  \
             enter_root \"$ROOT\" {} || VOID=\"the node's root could not be prepared\"\n  \
             ( env -i {} \"$UNSHARE\" -m --propagation private /bin/sh -c \"$IN_ROOT\" sh \"$ROOT\" \
             \"$CWD\" /.bohr/program {} >> \"$BASE/log-{index}\" 2>&1 & echo $! > \"$BASE/pid-{index}\"; \
             wait $!; echo $? > \"$BASE/status-{index}\" ) 2>/dev/null &\n  \
             for _ in $(seq 100); do [ -f \"$BASE/pid-{index}\" ] && break; sleep 0.01; done\n}}",
            quote(&argv[0]),
            vars.join(" "),
            words.join(" ")
        );
    }
    let program_paths: Vec<String> = plan.nodes.iter().map(|(argv, _)| quote(&argv[0])).collect();
    let _ = writeln!(
        out,
        "for p in {}; do [ -x \"$p\" ] || {{ echo \"no program at $p: run this where the run was started\"; exit 2; }}; done",
        program_paths.join(" ")
    );
    let expect: Vec<String> =
        plan.expect.iter().map(|(node, status)| format!("{node}:{status}")).collect();
    let _ = writeln!(out, "EXPECT=({})", expect.join(" "));
    let _ = writeln!(out, "STATE={}", plan.state.as_deref().map_or("''".to_owned(), quote));
    out.push_str(
        r#"
hits=0; voids=0
void_run() {
  kill_and_wait "$BASE"/pid-*; wait 2>/dev/null
  voids=$(( voids + 1 )); echo "run $run: void: $VOID"
}
for run in $(seq "$RUNS"); do
  VOID=''; EARLY=0
  rm -rf "$BASE"/pid-* "$BASE"/status-* "$BASE"/log-*
  [ "$POWER_LOSS" = 1 ] || rm -rf "$DISK"/node-*
  fresh_disk || { VOID="no fresh filesystem"; void_run; continue; }
"#,
    );
    for index in 0..plan.nodes.len() {
        let _ = writeln!(out, "  [ -z \"$STATE\" ] || cp -a \"$STATE\"/. \"$DISK/node-{index}\"/");
    }
    for index in 0..plan.nodes.len() {
        let _ = writeln!(out, "  start_{index}");
    }
    out.push_str("  [ -z \"$VOID\" ] || { void_run; continue; }\n  t0=$(date +%s%N); at=0\n");
    let mut previous = 0;
    for action in &plan.actions {
        // Gaps rather than absolute moments, so the faults keep their recorded order.
        let gap = action.at_ns.saturating_sub(previous);
        previous = action.at_ns;
        let _ = writeln!(
            out,
            "  w=$(( 2 * {gap} > SPREAD_NS ? 2 * {gap} : SPREAD_NS )); \
             at=$(( at + w * (RANDOM % 2001) / 2000 )); now=$(( $(date +%s%N) - t0 )); \
             [ \"$at\" -gt \"$now\" ] && sleep \"$(awk -v n=$(( at - now )) 'BEGIN {{ printf \"%.6f\", n / 1e9 }}')\""
        );
        let step = match action.what {
            "kill" => format!("up {0} && crash {0}", action.node),
            "freeze" => format!(
                "up {0} && kill -STOP \"$(cat \"$BASE/pid-{0}\")\" 2>/dev/null",
                action.node
            ),
            "thaw" => format!("kill -CONT \"$(cat \"$BASE/pid-{}\")\" 2>/dev/null", action.node),
            _ => format!("start_{}", action.node),
        };
        let _ = writeln!(out, "  {step}");
        let _ = writeln!(out, "  [ -z \"$VOID\" ] || {{ void_run; continue; }}");
    }
    out.push_str(
        r#"  # Until the run's horizon and, after the last fault, for as long as the run went on after it
  # and at least SPREAD_NS; or until the failure is there.
  now=$(( $(date +%s%N) - t0 )); rest=$(( REST_NS > SPREAD_NS ? REST_NS : SPREAD_NS ))
  deadline=$(( now + rest > HORIZON_NS ? now + rest : HORIZON_NS ))
  while [ $(( $(date +%s%N) - t0 )) -lt "$deadline" ]; do
    for e in "${EXPECT[@]}"; do
      [ "$(cat "$BASE/status-${e%%:*}" 2>/dev/null)" = "${e##*:}" ] && break 2
    done
    sleep 0.1
  done
  hit=$EARLY
  for e in "${EXPECT[@]}"; do
    [ "$(cat "$BASE/status-${e%%:*}" 2>/dev/null)" = "${e##*:}" ] && hit=1
  done
  hits=$(( hits + hit ))
  kill_and_wait "$BASE"/pid-*
  wait 2>/dev/null
  echo "run $run: $([ $hit = 1 ] && echo reproduced || echo not reproduced)"
  # What the failing node said last, so a reproduction can be told from another failure.
  if [ "$hit" = 1 ]; then
    for e in "${EXPECT[@]}"; do tail -n 3 "$BASE/log-${e%%:*}" 2>/dev/null | sed 's/^/    /'; done
  fi
done
echo "reproduced $hits of $(( RUNS - voids )) ($voids void)"
[ "$hits" -gt 0 ]
"#,
    );
    out
}

/// What a run's events say a host must do, cannot do, and should see: the faults it stages, in
/// order; what it cannot stage; and each failed node's exit status.
type ReadRun =
    (Vec<Action>, std::collections::BTreeSet<String>, std::collections::BTreeSet<(u32, i32)>);

fn read_run(events: &[bohr_trace::Event]) -> ReadRun {
    let mut actions = Vec::new();
    let mut unstaged = std::collections::BTreeSet::new();
    let mut expect = std::collections::BTreeSet::new();
    for event in events {
        let node = event.node.get();
        match &event.kind {
            EventKind::FaultFired { fault, target } => {
                let on = match target {
                    FaultTarget::Node(id) => id.get(),
                    _ => node,
                };
                let what = match fault {
                    FaultKind::NodeKill => "kill",
                    FaultKind::NodeFreeze => "freeze",
                    FaultKind::NodeThaw => "thaw",
                    // A restart from an older snapshot: the restart is staged from its
                    // lifecycle event below, the rewound disk is not.
                    FaultKind::NodeRestart => {
                        unstaged.insert("NodeRestart (a disk rewound to a snapshot)".to_owned());
                        continue;
                    }
                    other => {
                        unstaged.insert(format!("{other:?}"));
                        continue;
                    }
                };
                actions.push(Action { at_ns: event.tick.get(), node: on, what });
            }
            EventKind::NodeState { state: NodeState::Restarting } => {
                actions.push(Action { at_ns: event.tick.get(), node, what: "start" });
            }
            // A packet the draws lost, doubled, overtook or altered is not staged on a host,
            // whose network delivers what it is given.
            EventKind::PacketFate { fate, .. } if *fate != PacketFate::Delivered => {
                unstaged.insert(format!("{fate:?} packets"));
            }
            EventKind::TaskExited { status: ExitStatus::Exited(code) } if *code != 0 => {
                expect.insert((node, *code));
            }
            EventKind::TaskExited { status: ExitStatus::Signalled(signal) } => {
                expect.insert((node, 128 + i32::try_from(*signal).unwrap_or(0)));
            }
            _ => {}
        }
    }
    (actions, unstaged, expect)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An environment name `env` would read as an option and a working directory that climbs are
    /// refused.
    #[test]
    fn a_name_a_shell_could_read_another_way_is_refused() {
        assert!(check_env_key("RUST_LOG").is_ok());
        assert!(check_env_key("-S/bin/sh -c evil #").is_err());
        assert!(check_cwd("/srv/node").is_ok());
        assert!(check_cwd("/../../etc").is_err());
        assert!(check_cwd("relative").is_err());
    }

    /// A packet the draws lost is named among what a host does not stage.
    #[test]
    fn a_lost_packet_is_named_as_not_staged() {
        use bohr_trace::{Event, PacketFate};
        use bohr_types::{EventId, NodeId, TaskId, Tick};
        let event =
            |kind| Event { id: EventId(1), tick: Tick(1), node: NodeId(0), task: TaskId(0), kind };
        let events = [
            event(EventKind::PacketFate { fate: PacketFate::Dropped, delay_ticks: 0 }),
            event(EventKind::PacketFate { fate: PacketFate::Delivered, delay_ticks: 0 }),
        ];
        let (_, unstaged, _) = read_run(&events);
        assert_eq!(unstaged.into_iter().collect::<Vec<_>>(), vec!["Dropped packets".to_owned()]);
    }

    #[test]
    fn a_word_is_quoted_for_the_shell() {
        assert_eq!(quote("/data"), "'/data'");
        assert_eq!(quote("it's"), "'it'\\''s'");
    }
}
