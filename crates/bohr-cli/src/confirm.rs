//! `bohr confirm`: writes a script that stages a stored finding on a plain Linux host.
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
use bohr_trace::{EventKind, ExitStatus, FaultKind, FaultTarget, NodeState};

use crate::CliError;

/// A node's argument vector and environment, as the scenario gave them.
type Node = (Vec<String>, Vec<(String, String)>);

/// What the script stages.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Each node's argument vector and environment.
    nodes: Vec<Node>,
    /// The directory every node's filesystem started from, where the scenario prepared one.
    state: Option<String>,
    /// The working directory every node starts in.
    cwd: String,
    /// The faults, in order of virtual time.
    actions: Vec<Action>,
    /// How long the run was, in nanoseconds of virtual time.
    horizon_ns: u64,
    /// How long the run went on after its last fault, in nanoseconds of virtual time.
    rest_ns: u64,
    /// The exit statuses that make the finding, as `(node, status)`; 128 + n for signal n.
    expect: Vec<(u32, i32)>,
    /// Faults the run delivered that the script cannot stage.
    unstaged: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct Action {
    at_ns: u64,
    node: u32,
    what: &'static str,
}

impl Plan {
    /// The plan for a stored run, or why it has none.
    pub(crate) fn of(model: &RunModel) -> Result<Self, String> {
        let config: toml::Table = model
            .config
            .parse()
            .map_err(|e| format!("the run's configuration does not parse: {e}"))?;
        let cluster = config.get("cluster").and_then(toml::Value::as_table);
        let count = cluster
            .and_then(|c| c.get("nodes"))
            .and_then(toml::Value::as_integer)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(1);
        let strings = |value: Option<&toml::Value>| -> Vec<String> {
            value
                .and_then(toml::Value::as_array)
                .map(|items| items.iter().filter_map(|w| w.as_str().map(str::to_owned)).collect())
                .unwrap_or_default()
        };
        let mut template = strings(cluster.and_then(|c| c.get("argv")));
        let programs: Vec<Vec<String>> = cluster
            .and_then(|c| c.get("programs"))
            .and_then(toml::Value::as_array)
            .map(|all| all.iter().map(|one| strings(Some(one))).collect())
            .unwrap_or_default();
        // A program given on the command line is in the bundle's record of what it started.
        if template.is_empty()
            && programs.is_empty()
            && let Some(first) = model.binaries.iter().find(|b| b.role == "executable")
        {
            template = std::iter::once(first.path.clone()).chain(first.args.clone()).collect();
        }
        let env: Vec<(String, String)> = cluster
            .and_then(|c| c.get("env"))
            .and_then(toml::Value::as_table)
            .map(|env| {
                env.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        let mut nodes = Vec::new();
        for index in 0..count {
            let argv: Vec<String> = programs
                .get(index as usize)
                .unwrap_or(&template)
                .iter()
                .map(|word| word.replace("{node}", &index.to_string()))
                .collect();
            if argv.is_empty() || argv[0].starts_with("builtin:") {
                return Err("the run names no program for a node; a run of a built-in guest has \
                            nothing to start on a host"
                    .to_owned());
            }
            nodes.push((argv, env.clone()));
        }
        let prepared = config.get("state");
        let state =
            prepared.and_then(|s| s.get("dir")).and_then(toml::Value::as_str).map(str::to_owned);
        let cwd = prepared
            .and_then(|s| s.get("cwd"))
            .and_then(toml::Value::as_str)
            .unwrap_or("/")
            .to_owned();

        let mut actions = Vec::new();
        let mut unstaged = std::collections::BTreeSet::new();
        let mut expect = std::collections::BTreeSet::new();
        for event in &model.events {
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
                            unstaged
                                .insert("NodeRestart (a disk rewound to a snapshot)".to_owned());
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
                EventKind::TaskExited { status: ExitStatus::Exited(code) } if *code != 0 => {
                    expect.insert((node, *code));
                }
                EventKind::TaskExited { status: ExitStatus::Signalled(signal) } => {
                    expect.insert((node, 128 + i32::try_from(*signal).unwrap_or(0)));
                }
                _ => {}
            }
        }
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

    fn crashes(&self) -> bool {
        self.actions.iter().any(|a| a.what == "kill")
    }
}

/// `text` as a single-quoted shell word.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The script for `plan`; `bundle` is the name it prints.
pub(crate) fn script(plan: &Plan, bundle: &str, runs: u32) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "#!/usr/bin/env bash\n\
         # Written by `bohr confirm {bundle}`. Stages the recorded run on this host without bohr.\n\
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

/// Writes the script for the bundle at `path` to `out`.
pub(crate) fn confirm(path: &str, out: &str, runs: u32) -> Result<u8, CliError> {
    let bytes = std::fs::read(path)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot read {path}: {e}")))?;
    let bundle = bohr_trace::Bundle::parse(&bytes).map_err(|e| CliError::Usage(e.to_string()))?;
    let model = RunModel::of(&bundle).map_err(|e| CliError::Usage(e.to_string()))?;
    let plan =
        Plan::of(&model).map_err(|why| CliError::Usage(format!("BOHR-5003: {path}: {why}")))?;
    let text = script(&plan, path, runs);
    std::fs::write(out, &text)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot write {out}: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o755));
    }
    if crate::json::on() {
        crate::json::emit(
            "confirm",
            serde_json::json!({
                "bundle": path,
                "written": out,
                "nodes": plan.nodes.len(),
                "faults": plan.actions.len(),
                "power_loss": plan.crashes(),
                "expect": plan.expect.iter().map(|(n, s)| serde_json::json!({ "node": n, "status": s })).collect::<Vec<_>>(),
                "not_staged": plan.unstaged,
                "runs": runs,
            }),
        );
    } else {
        println!("{out}");
        if plan.crashes() {
            eprintln!(
                "  the run crashed a node: run the script as root, which it needs for dm-flakey"
            );
        }
        if !plan.unstaged.is_empty() {
            eprintln!("  not staged, though the run had them: {}", plan.unstaged.join(", "));
        }
    }
    Ok(crate::exit::CLEAN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_is_quoted_for_the_shell() {
        assert_eq!(quote("/data"), "'/data'");
        assert_eq!(quote("it's"), "'it'\\''s'");
    }
}
