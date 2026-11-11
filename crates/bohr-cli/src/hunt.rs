//! `bohr hunt <pack>`: a campaign against a real client, from one command.
//!
//! Resolves the pack, accepts its binary only if the digest matches (building it if missing), and
//! runs the pack's scenario through `bohr explore`, writing everything to one directory. With
//! `--seeds a..b` it runs those seeds once each instead. Findings are grouped at the end.
//!
//! The pack's scenario and oracle decide what is searched for; a clean hunt says nothing beyond
//! that scenario.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

/// Where a run is told its position among the cores, as `bohr explore` tells its workers.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use bohr_backend_ptrace::CORE_VARIABLE;

use crate::{CliError, private::EXECUTE_VARIABLE, targets};
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
const CORE_VARIABLE: &str = "BOHR_CORE";

/// Everything a hunt was asked for.
pub(crate) struct Hunt {
    pub(crate) pack: String,
    pub(crate) time: Option<Duration>,
    pub(crate) runs: Option<u64>,
    pub(crate) quiet_runs: Option<u64>,
    pub(crate) jobs: usize,
    pub(crate) seeds: Option<(u64, u64)>,
    pub(crate) no_faults: bool,
    pub(crate) out: PathBuf,
    pub(crate) resume: bool,
    pub(crate) binary: Option<String>,
    pub(crate) master_seed: u64,
    /// The scenario to hunt under, where not the pack's own: one of its cells.
    pub(crate) scenario: Option<PathBuf>,
}

/// Hunts every cell of a pack: each `scenario-s*.toml` beside the pack's scenario, one after
/// another, each into a directory of its own under the output, and says what each came to.
///
/// The worst exit of the cells is the hunt's.
pub(crate) fn hunt_cells(request: &Hunt) -> Result<u8, CliError> {
    if crate::json::on() {
        return Err(CliError::usage(
            "BOHR-1001: --cells runs one hunt a cell, and --json writes one object; run a cell at a \
             time for its object"
                .to_owned(),
        ));
    }
    let pack = targets::resolve(&request.pack)?;
    let dir = pack.scenario.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut cells: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| CliError::refused(format!("cannot list {}: {e}", dir.display())))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.starts_with("scenario-s") && name.ends_with(".toml"))
        })
        .collect();
    cells.sort();
    if cells.is_empty() {
        return Err(CliError::refused(format!(
            "pack {} has no cells: no scenario-s*.toml beside {}",
            pack.name,
            pack.scenario.display()
        )));
    }
    let mut worst = crate::exit::CLEAN;
    let mut said = Vec::new();
    for cell in &cells {
        let name = cell
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("cell")
            .trim_start_matches("scenario-");
        eprintln!("\ncell {name}: {}", cell.display());
        let code = hunt(&Hunt {
            out: request.out.join(name),
            scenario: Some(cell.clone()),
            pack: request.pack.clone(),
            binary: request.binary.clone(),
            ..*request
        })
        .unwrap_or_else(|e| {
            eprintln!("cell {name}: {e}");
            e.exit()
        });
        worst = worst.max(code);
        said.push(format!("  {name:<8} exit {code} · {}", request.out.join(name).display()));
    }
    eprintln!("\ncells of {}", pack.name);
    for line in said {
        eprintln!("{line}");
    }
    Ok(worst)
}

/// A hunt with no budget stops after this many runs that added no new group of findings.
const DEFAULT_QUIET_RUNS: u64 = 1000;

/// A hunt with no budget stops after this long, whatever it is still finding.
const DEFAULT_TIME: Duration = Duration::from_secs(15 * 60);

/// A seed range as `a..b`, both ends included.
pub(crate) fn parse_seeds(text: &str) -> Result<(u64, u64), String> {
    let (from, to) = text.split_once("..").ok_or_else(|| format!("{text:?} is not a..b"))?;
    let from: u64 = from.trim().parse().map_err(|_| format!("{from:?} is not a seed"))?;
    let to: u64 = to.trim().parse().map_err(|_| format!("{to:?} is not a seed"))?;
    if to < from {
        return Err(format!("{text:?} ends before it starts"));
    }
    Ok((from, to))
}

/// The scenario with every drawn and scheduled fault removed. An oracle that fires here is judging
/// the client's normal behaviour.
///
/// `[durability]` is removed too: it judges what a restarted node lost, which needs a kill.
pub(crate) fn without_faults(text: &str) -> Result<String, CliError> {
    let mut value: toml::Table =
        toml::from_str(text).map_err(|e| CliError::refused(format!("the pack's scenario: {e}")))?;
    value.remove("faults");
    value.remove("fault");
    value.remove("durability");
    value.remove("partition");
    value.remove("grey");
    for (section, keys) in [
        (
            "network",
            &[
                "drop_probability",
                "duplicate_probability",
                "reorder_probability",
                "corrupt_probability",
            ][..],
        ),
        ("disk", &["stall_probability", "eio_probability", "enospc_probability"][..]),
    ] {
        if let Some(toml::Value::Table(table)) = value.get_mut(section) {
            for key in keys {
                table.remove(*key);
            }
        }
    }
    toml::to_string(&value)
        .map_err(|e| CliError::refused(format!("cannot write the scenario: {e}")))
}

/// Every name the scenario starts the pack's binary by, each of which a `--binary` stands in for.
///
/// Refused where the scenario never names the pack's binary, since the stand-in would not run.
fn named_binary(text: &str, pack: &targets::Pack) -> Result<Vec<String>, CliError> {
    let value: toml::Table =
        toml::from_str(text).map_err(|e| CliError::refused(format!("the pack's scenario: {e}")))?;
    let ours = |arg: &toml::Value| {
        arg.as_str()
            .filter(|p| pack.root.join(p) == pack.binary || Path::new(p) == pack.binary)
            .map(str::to_owned)
    };
    let cluster = value.get("cluster").and_then(toml::Value::as_table);
    let argv = cluster.and_then(|c| c.get("argv")).and_then(toml::Value::as_array);
    let programs = cluster.and_then(|c| c.get("programs")).and_then(toml::Value::as_array);
    let firsts = argv
        .and_then(|argv| argv.first())
        .into_iter()
        .chain(programs.into_iter().flatten().filter_map(|p| p.as_array()?.first()));
    let mut names: Vec<String> = firsts.filter_map(ours).collect();
    names.sort();
    names.dedup();
    if !names.is_empty() {
        return Ok(names);
    }
    Err(CliError::refused(format!(
        "pack {}'s scenario names {} nowhere in its argv, so --binary would not be what runs",
        pack.name,
        pack.binary.display()
    )))
}

/// The pack's binary as the command line names it: from the pack's root, where the guest runs, so
/// no host directory reaches the guest.
fn as_started(pack: &targets::Pack) -> String {
    match pack.binary.strip_prefix(&pack.root) {
        Ok(inside) => format!("./{}", inside.to_string_lossy()),
        Err(_) => absolute(&pack.binary),
    }
}

/// Runs a hunt and reports what it found.
pub(crate) fn hunt(request: &Hunt) -> Result<u8, CliError> {
    let pack = targets::resolve(&request.pack)?;
    let binary = targets::binary_for(&pack, request.binary.as_deref().map(Path::new))?;
    std::fs::create_dir_all(&request.out)
        .map_err(|e| CliError::refused(format!("cannot use {}: {e}", request.out.display())))?;
    let state = request.out.join("campaign");
    if request.seeds.is_none() && state.join("campaign.bin").is_file() && !request.resume {
        return Err(CliError::refused(format!(
            "{} already holds a campaign; pass --resume to carry on with it, or name another -o",
            request.out.display()
        )));
    }

    // The scenario must arm the pack's oracle, or every run would be clean for lack of checking.
    let scenario_path = request.scenario.clone().unwrap_or_else(|| pack.scenario.clone());
    let text = std::fs::read_to_string(&scenario_path)
        .map_err(|e| CliError::refused(format!("cannot read {}: {e}", scenario_path.display())))?;
    let loaded = crate::state_dir::load(&text)?;
    let armed = match pack.oracle.as_str() {
        "durability" => loaded.durability.is_some(),
        "liveness" => loaded.liveness.is_some(),
        "chain" => !loaded.probes.is_empty(),
        "laws" => loaded.model.is_some(),
        _ => true,
    };
    if !armed {
        return Err(CliError::refused(format!(
            "pack {} judges by {}, and its scenario has nothing to judge with: {}",
            pack.name,
            pack.oracle,
            match pack.oracle.as_str() {
                "chain" => "no [chain] and no [[probe]]",
                "laws" => "no [model]",
                "durability" => "no [durability]",
                _ => "no [liveness]",
            }
        )));
    }
    // The binary checked is the binary run: a `--binary` is executed wherever the scenario starts
    // the pack's, and the guest still sees the pack's name for it.
    let (program, stand_in) = match request.binary {
        Some(_) => {
            let named = named_binary(&text, &pack)?;
            let file = absolute(&binary);
            // This process judges the findings too, so it runs the stand-in as its workers do.
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            for name in &named {
                crate::private::substitute(name, &file);
            }
            let stand_in: Vec<String> = named.iter().map(|name| format!("{name}={file}")).collect();
            (named[0].clone(), Some(stand_in.join("\n")))
        }
        None => (as_started(&pack), None),
    };
    let mut text = text;
    if request.no_faults {
        text = without_faults(&text)?;
    }
    let scenario = if request.no_faults {
        let written = request.out.join("scenario.toml");
        std::fs::write(&written, &text)
            .map_err(|e| CliError::refused(format!("cannot write {}: {e}", written.display())))?;
        written
    } else {
        scenario_path.clone()
    };
    eprintln!(
        "hunting {} ({} at {}) · blake3 {} · judged by {} · {}",
        pack.name,
        pack.repository,
        pack.commit,
        &pack.digest[..12.min(pack.digest.len())],
        pack.oracle,
        scenario.display()
    );

    let me =
        std::env::current_exe().map_err(|e| CliError::refused(format!("cannot find bohr: {e}")))?;
    let findings = request.out.join("findings");
    let summary = request.out.join("summary.json");
    let (code, swept) = match request.seeds {
        Some((from, to)) => {
            let swept = sweep(
                &me,
                &scenario,
                &program,
                stand_in.as_deref(),
                &pack.root,
                &findings,
                from,
                to,
                request.jobs,
            )?;
            (swept.code(), Some(swept))
        }
        None => {
            let mut args: Vec<String> = vec![
                "explore".into(),
                "-j".into(),
                request.jobs.to_string(),
                "--master-seed".into(),
                request.master_seed.to_string(),
                "--findings".into(),
                absolute(&findings),
                "--resume".into(),
                absolute(&state),
                "--summary".into(),
                absolute(&summary),
                "--config".into(),
                absolute(&scenario),
            ];
            // Without a budget a hunt is a short pass that stops when it has nothing new: most of
            // what a campaign finds, it finds in its first runs.
            let unbudgeted =
                request.time.is_none() && request.runs.is_none() && request.quiet_runs.is_none();
            let (time, quiet) = if unbudgeted {
                (Some(DEFAULT_TIME), Some(DEFAULT_QUIET_RUNS))
            } else {
                (request.time, request.quiet_runs)
            };
            if let Some(time) = time {
                args.extend(["--time".into(), format!("{}s", time.as_secs().max(1))]);
            }
            if let Some(runs) = request.runs {
                args.extend(["--runs".into(), runs.to_string()]);
            }
            if let Some(quiet) = quiet {
                args.extend(["--quiet-runs".into(), quiet.to_string()]);
            }
            args.extend(["--".into(), program.clone()]);
            let mut campaign = Command::new(&me);
            campaign.args(&args).current_dir(&pack.root);
            if let Some(stand_in) = &stand_in {
                campaign.env(EXECUTE_VARIABLE, stand_in);
            }
            // Standard output carries only the object, so the campaign's output goes to stderr.
            if crate::json::on() {
                campaign.stdout(std::io::stderr());
            }
            let status = campaign
                .status()
                .map_err(|e| CliError::refused(format!("cannot start the campaign: {e}")))?;
            let code =
                status.code().and_then(|c| u8::try_from(c).ok()).unwrap_or(crate::exit::INTERNAL);
            (code, None)
        }
    };
    // Judged where the campaign ran, so the relative programs and state its bundles name are
    // the ones it ran.
    let findings = std::path::absolute(&findings).unwrap_or(findings);
    if !crate::json::on() {
        if findings.is_dir() {
            // Ablated: a campaign's groups are few, and a verdict without it is a guess.
            from_root(&pack.root, || crate::findings::findings(&findings.to_string_lossy(), true))?;
        }
        return Ok(code);
    }

    // The campaign's account, or null if it stopped before writing one.
    let campaign = request.seeds.is_none().then(|| {
        std::fs::read_to_string(&summary)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    });
    let mut body = serde_json::json!({
        "pack": pack.name,
        "repository": pack.repository,
        "commit": pack.commit,
        "digest": pack.digest,
        "oracle": pack.oracle,
        "binary": binary.display().to_string(),
        "scenario": scenario.display().to_string(),
        "out": request.out.display().to_string(),
        "mode": if request.seeds.is_some() { "seeds" } else { "campaign" },
        "campaign": campaign.flatten(),
        "summary": request.seeds.is_none().then(|| summary.display().to_string()),
        "sweep": swept.as_ref().map(Swept::to_json),
        "findings_dir": findings.display().to_string(),
        "findings": 0,
        "groups": [],
        "exit": code,
    });
    // The groups `bohr findings` would print, inside this object.
    if findings.is_dir()
        && let (serde_json::Value::Object(fields), serde_json::Value::Object(groups)) = (
            &mut body,
            from_root(&pack.root, || {
                crate::findings::groups_json(&findings.to_string_lossy(), true)
            })?,
        )
    {
        fields.extend(groups);
    }
    crate::json::emit("hunt", body);
    Ok(code)
}

/// Runs `judge` from `root`, the directory the campaign ran in, and comes back.
///
/// A finding is run again to be ablated, and its bundle names programs and a prepared state as the
/// scenario wrote them, relative to the pack's root. The process's directory is moved for the
/// span of the call; a hunt does nothing else meanwhile.
fn from_root<T>(root: &Path, judge: impl FnOnce() -> Result<T, CliError>) -> Result<T, CliError> {
    let here = std::env::current_dir()
        .map_err(|e| CliError::refused(format!("cannot read the working directory: {e}")))?;
    std::env::set_current_dir(root)
        .map_err(|e| CliError::refused(format!("cannot enter {}: {e}", root.display())))?;
    let judged = judge();
    std::env::set_current_dir(&here)
        .map_err(|e| CliError::refused(format!("cannot return to {}: {e}", here.display())))?;
    judged
}

/// Keeps the account of the smallest seed of those offered, so the report names the same one
/// whichever thread finished first.
fn keep_smallest(slot: &mut Option<(u64, String)>, seed: u64, said: String) {
    if slot.as_ref().is_none_or(|(at, _)| seed < *at) {
        *slot = Some((seed, said));
    }
}

fn absolute(path: &Path) -> String {
    std::path::absolute(path).unwrap_or_else(|_| path.to_owned()).to_string_lossy().into_owned()
}

/// What a sweep of a seed range came to, seed by seed.
#[derive(Debug, Default, PartialEq, Eq)]
struct Swept {
    from: u64,
    to: u64,
    /// Seeds whose run found something; their bundles are kept.
    found: Vec<u64>,
    /// Seeds whose run did not complete.
    incomplete: Vec<u64>,
    /// Why the first seed that did not complete did not, in its own last line.
    why: Option<String>,
}

impl Swept {
    fn seeds(&self) -> u64 {
        self.to - self.from + 1
    }

    fn clean(&self) -> u64 {
        self.seeds() - self.found.len() as u64 - self.incomplete.len() as u64
    }

    fn code(&self) -> u8 {
        if !self.incomplete.is_empty() {
            crate::exit::INTERNAL
        } else if !self.found.is_empty() {
            crate::exit::FINDING
        } else {
            crate::exit::CLEAN
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "from": self.from,
            "to": self.to,
            "seeds": self.seeds(),
            "clean": self.clean(),
            "found": self.found,
            "incomplete": self.incomplete,
            "first_incomplete": self.why,
        })
    }
}

/// Runs each seed of a range once, keeping the bundle of every run that found something.
#[allow(clippy::too_many_arguments)]
fn sweep(
    me: &Path,
    scenario: &Path,
    program: &str,
    stand_in: Option<&str>,
    root: &Path,
    findings: &Path,
    from: u64,
    to: u64,
    jobs: usize,
) -> Result<Swept, CliError> {
    std::fs::create_dir_all(findings)
        .map_err(|e| CliError::refused(format!("cannot use {}: {e}", findings.display())))?;
    let seeds: Vec<u64> = (from..=to).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let found: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());
    let other: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());
    // Why the first incomplete seed stopped, so nobody has to rerun it to find out.
    // The smallest seed that did not finish, whichever thread met it first.
    let why: std::sync::Mutex<Option<(u64, String)>> = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for position in 0..jobs.max(1) {
            let (next, found, other, why) = (&next, &found, &other, &why);
            let seeds = &seeds;
            scope.spawn(move || {
                loop {
                    let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(seed) = seeds.get(at) else { break };
                    let bundle = findings.join(format!("seed-{seed}.bohr"));
                    let run = |save: Option<&Path>| {
                        let mut command = Command::new(me);
                        command
                            .args(["run", "--seed", &seed.to_string(), "--config"])
                            .arg(absolute(scenario));
                        if let Some(bundle) = save {
                            command.arg("--save").arg(bundle);
                        }
                        if let Some(stand_in) = stand_in {
                            command.env(EXECUTE_VARIABLE, stand_in);
                        }
                        command
                            .arg("--")
                            .arg(program)
                            .current_dir(root)
                            // A core of its own per sweeping thread, as a campaign gives each
                            // worker.
                            .env(CORE_VARIABLE, position.to_string())
                            .output()
                    };
                    // Only digested; a seed that found something runs again, to the same end,
                    // with its bundle written, so a clean seed never spools a log.
                    let first = run(None);
                    let found_it = first
                        .as_ref()
                        .ok()
                        .and_then(|o| o.status.code())
                        .is_some_and(|c| c == i32::from(crate::exit::FINDING));
                    let status = if found_it { run(Some(&bundle)) } else { first };
                    let said = status.as_ref().map_or_else(
                        |e| e.to_string(),
                        |o| {
                            String::from_utf8_lossy(&o.stderr)
                                .lines()
                                .last()
                                .unwrap_or("")
                                .to_owned()
                        },
                    );
                    match status
                        .ok()
                        .and_then(|o| o.status.code())
                        .and_then(|c| u8::try_from(c).ok())
                    {
                        Some(crate::exit::FINDING) => {
                            if let Ok(mut found) = found.lock() {
                                found.push(*seed);
                            }
                        }
                        Some(crate::exit::CLEAN) => {
                            let _ = std::fs::remove_file(&bundle);
                        }
                        _ => {
                            if let Ok(mut other) = other.lock() {
                                other.push(*seed);
                            }
                            if let Ok(mut first) = why.lock() {
                                keep_smallest(&mut first, *seed, format!("seed {seed}: {said}"));
                            }
                            let _ = std::fs::remove_file(&bundle);
                        }
                    }
                }
            });
        }
    });
    let mut swept = Swept {
        from,
        to,
        found: found.into_inner().unwrap_or_default(),
        incomplete: other.into_inner().unwrap_or_default(),
        why: why.into_inner().ok().flatten().map(|(_, said)| said),
    };
    // Sorted: threads finish seeds in any order.
    swept.found.sort_unstable();
    swept.incomplete.sort_unstable();
    if let Some(said) = &swept.why {
        eprintln!("  {said}");
    }
    eprintln!(
        "swept {from}..{to}: {} clean, {} findings, {} that did not complete",
        swept.clean(),
        swept.found.len(),
        swept.incomplete.len()
    );
    Ok(swept)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The incomplete seed named is the smallest, in whatever order the threads met them.
    #[test]
    fn the_smallest_incomplete_seed_is_named() {
        let mut slot = None;
        for seed in [40, 5, 17] {
            keep_smallest(&mut slot, seed, format!("seed {seed}"));
        }
        assert_eq!(slot.map(|(seed, _)| seed), Some(5));
    }

    #[test]
    fn a_seed_range_includes_both_ends() {
        assert_eq!(parse_seeds("1..60"), Ok((1, 60)));
        assert_eq!(parse_seeds("7..7"), Ok((7, 7)));
        assert!(parse_seeds("9..3").is_err());
        assert!(parse_seeds("9").is_err());
    }

    /// A sweep's object names every seed that found something and every one that did not complete,
    /// and a seed that did not complete outranks a finding in the exit code.
    #[test]
    fn a_sweep_reads_seed_by_seed() {
        let swept = Swept {
            from: 1,
            to: 6,
            found: vec![2, 5],
            incomplete: vec![4],
            why: Some("seed 4: killed".to_owned()),
        };
        let value = crate::json::object("hunt", serde_json::json!({ "sweep": swept.to_json() }));
        assert_eq!(value["schema"], "bohr.hunt/1");
        assert_eq!(value["sweep"]["seeds"], 6);
        assert_eq!(value["sweep"]["clean"], 3);
        assert_eq!(value["sweep"]["found"], serde_json::json!([2, 5]));
        assert_eq!(value["sweep"]["incomplete"], serde_json::json!([4]));
        assert_eq!(value["sweep"]["first_incomplete"], "seed 4: killed");
        assert_eq!(swept.code(), crate::exit::INTERNAL);
        assert_eq!(Swept { incomplete: vec![], ..swept }.code(), crate::exit::FINDING);
    }

    /// Every way a scenario arms adversity is taken out, and nothing else is.
    #[test]
    fn no_faults_leaves_the_world_and_takes_the_faults() {
        let text = r#"
[run]
horizon_ticks = 5
[cluster]
nodes = 2
[network]
latency_mean_ticks = 10
drop_probability = "1/10"
corrupt_probability = "1/50"
[disk]
stall_probability = "1/16"
snapshot_depth = 2
[durability]
ack = "send_while_dirty"
[faults]
kill_probability = "1/20"
[[fault]]
kind = "node_kill"
node = 0
at_tick = 3
"#;
        let out = without_faults(text).unwrap();
        let loaded = crate::state_dir::load(&out).unwrap();
        assert!(!loaded.asks_for_adversity(), "{out}");
        assert!(out.contains("latency_mean_ticks"), "the world stays: {out}");
        assert!(out.contains("snapshot_depth"), "{out}");
        assert!(loaded.node_faults.is_empty());
        assert!(loaded.durability.is_none(), "nothing is killed, so nothing is judged durable");
    }
}
