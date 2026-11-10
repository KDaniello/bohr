//! `bohr hunt <pack>`: a campaign against a real client, from one command.
//!
//! Resolves the pack, takes its binary only if the digest matches (building it where it is
//! missing), and runs the pack's scenario as a campaign: `bohr explore` by this very binary, so
//! a hunt is the same search a person could have typed out, written to one directory. With
//! `--seeds a..b` it runs those seeds once each instead, which is how a known defect is checked
//! for on a range a person chose. Findings are grouped at the end.
//!
//! What this does not do: choose what to search for. The pack's scenario and oracle decide that,
//! and a hunt that finds nothing has measured that scenario and nothing else.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

/// Where a run is told its position among the cores, as `bohr explore` tells its workers.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use bohr_backend_ptrace::CORE_VARIABLE;

use crate::{CliError, targets};
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
const CORE_VARIABLE: &str = "BOHR_CORE";

/// Everything a hunt was asked for.
pub(crate) struct Hunt {
    pub(crate) pack: String,
    pub(crate) time: Option<Duration>,
    pub(crate) runs: Option<u64>,
    pub(crate) jobs: usize,
    pub(crate) seeds: Option<(u64, u64)>,
    pub(crate) no_faults: bool,
    pub(crate) out: PathBuf,
    pub(crate) resume: bool,
    pub(crate) binary: Option<String>,
    pub(crate) master_seed: u64,
}

fn refused(text: String) -> CliError {
    CliError::Usage(format!("BOHR-1002: {text}"))
}

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

/// The scenario with every drawn and scheduled fault taken out, for checking an oracle on runs
/// nothing was done to: a pack whose oracle fires here is judging the client's normal behaviour.
///
/// `[durability]` goes with the faults: it judges what a restarted node came back without, a run
/// with no kill has nothing for it to judge, and a scenario arming it without one is refused.
pub(crate) fn without_faults(text: &str) -> Result<String, CliError> {
    let mut value: toml::Table =
        toml::from_str(text).map_err(|e| refused(format!("the pack's scenario: {e}")))?;
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
    toml::to_string(&value).map_err(|e| refused(format!("cannot write the scenario: {e}")))
}

/// The scenario with every program that is the pack's binary replaced by `binary`.
///
/// Refused where the scenario names the pack's binary nowhere: a scenario whose argv runs another
/// program would run that program whatever was checked.
fn with_program(text: &str, pack: &targets::Pack, binary: &Path) -> Result<String, CliError> {
    let mut value: toml::Table =
        toml::from_str(text).map_err(|e| refused(format!("the pack's scenario: {e}")))?;
    let ours = |arg: &toml::Value| {
        arg.as_str()
            .is_some_and(|p| pack.root.join(p) == pack.binary || Path::new(p) == pack.binary)
    };
    let given = toml::Value::String(absolute(binary));
    let mut replaced = 0;
    if let Some(toml::Value::Table(cluster)) = value.get_mut("cluster") {
        let mut lines: Vec<&mut Vec<toml::Value>> = Vec::new();
        if let Some(toml::Value::Array(argv)) = cluster.get_mut("argv") {
            lines.push(argv);
        }
        // `programs` and `argv` cannot both be borrowed at once, so each is taken in turn.
        for argv in lines {
            if argv.first().is_some_and(ours) {
                argv[0] = given.clone();
                replaced += 1;
            }
        }
        if let Some(toml::Value::Array(programs)) = cluster.get_mut("programs") {
            for program in programs.iter_mut() {
                if let toml::Value::Array(argv) = program
                    && argv.first().is_some_and(ours)
                {
                    argv[0] = given.clone();
                    replaced += 1;
                }
            }
        }
    }
    if replaced == 0 {
        return Err(refused(format!(
            "pack {}'s scenario names {} nowhere in its argv, so --binary would not be what runs",
            pack.name,
            pack.binary.display()
        )));
    }
    toml::to_string(&value).map_err(|e| refused(format!("cannot write the scenario: {e}")))
}

/// Runs a hunt and reports what it found.
pub(crate) fn hunt(request: &Hunt) -> Result<u8, CliError> {
    let pack = targets::resolve(&request.pack)?;
    let binary = targets::binary_for(&pack, request.binary.as_deref().map(Path::new))?;
    std::fs::create_dir_all(&request.out)
        .map_err(|e| refused(format!("cannot use {}: {e}", request.out.display())))?;
    let state = request.out.join("campaign");
    if request.seeds.is_none() && state.join("campaign.bin").is_file() && !request.resume {
        return Err(refused(format!(
            "{} already holds a campaign; pass --resume to carry on with it, or name another -o",
            request.out.display()
        )));
    }

    // **The oracle a pack names has to be armed by its scenario.** A durability or liveness pack
    // whose scenario does not watch for it would report every run clean for want of looking.
    let text = std::fs::read_to_string(&pack.scenario)
        .map_err(|e| refused(format!("cannot read {}: {e}", pack.scenario.display())))?;
    let loaded = bohr_world::config::load(&text)?;
    let armed = match pack.oracle.as_str() {
        "durability" => loaded.durability.is_some(),
        "liveness" => loaded.liveness.is_some(),
        "chain" => !loaded.probes.is_empty(),
        _ => true,
    };
    if !armed {
        return Err(refused(format!(
            "pack {} judges by {}, and its scenario has nothing to judge with: {}",
            pack.name,
            pack.oracle,
            match pack.oracle.as_str() {
                "chain" => "no [chain] and no [[probe]]",
                "durability" => "no [durability]",
                _ => "no [liveness]",
            }
        )));
    }
    // **The binary checked is the binary run.** A scenario names its program in its own argv, and
    // the argv is what a run starts; a binary given on the command line replaces the pack's there,
    // or the digest checked would belong to one file and the run to another.
    let mut text = text;
    if request.binary.is_some() {
        text = with_program(&text, &pack, &binary)?;
    }
    if request.no_faults {
        text = without_faults(&text)?;
    }
    let scenario = if request.binary.is_some() || request.no_faults {
        let written = request.out.join("scenario.toml");
        std::fs::write(&written, &text)
            .map_err(|e| refused(format!("cannot write {}: {e}", written.display())))?;
        written
    } else {
        pack.scenario.clone()
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

    let me = std::env::current_exe().map_err(|e| refused(format!("cannot find bohr: {e}")))?;
    let findings = request.out.join("findings");
    let summary = request.out.join("summary.json");
    let (code, swept) = match request.seeds {
        Some((from, to)) => {
            let swept =
                sweep(&me, &scenario, &binary, &pack.root, &findings, from, to, request.jobs)?;
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
            if let Some(time) = request.time {
                args.extend(["--time".into(), format!("{}s", time.as_secs().max(1))]);
            }
            if let Some(runs) = request.runs {
                args.extend(["--runs".into(), runs.to_string()]);
            }
            if request.time.is_none() && request.runs.is_none() {
                return Err(CliError::Usage(
                    "BOHR-1001: a hunt needs a budget: --time, --runs or --seeds".to_owned(),
                ));
            }
            args.extend(["--".into(), absolute(&binary)]);
            let mut campaign = Command::new(&me);
            campaign.args(&args).current_dir(&pack.root);
            // Standard output belongs to the one object, so whatever the campaign prints goes
            // where its progress goes.
            if crate::json::on() {
                campaign.stdout(std::io::stderr());
            }
            let status = campaign
                .status()
                .map_err(|e| refused(format!("cannot start the campaign: {e}")))?;
            let code =
                status.code().and_then(|c| u8::try_from(c).ok()).unwrap_or(crate::exit::INTERNAL);
            (code, None)
        }
    };
    if !crate::json::on() {
        if findings.is_dir() {
            crate::findings::findings(&findings.to_string_lossy())?;
        }
        return Ok(code);
    }

    // The campaign's own account, where it wrote one: a campaign that stopped before it wrote
    // anything has a null here rather than an account of nothing.
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
    // The groups `bohr findings` would print, inside this object rather than as a second one.
    if findings.is_dir()
        && let (serde_json::Value::Object(fields), serde_json::Value::Object(groups)) =
            (&mut body, crate::findings::groups_json(&findings.to_string_lossy())?)
    {
        fields.extend(groups);
    }
    crate::json::emit("hunt", body);
    Ok(code)
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
    binary: &Path,
    root: &Path,
    findings: &Path,
    from: u64,
    to: u64,
    jobs: usize,
) -> Result<Swept, CliError> {
    std::fs::create_dir_all(findings)
        .map_err(|e| refused(format!("cannot use {}: {e}", findings.display())))?;
    let seeds: Vec<u64> = (from..=to).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let found: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());
    let other: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());
    // Why the first seed that did not complete did not, because a count alone sends a reader off
    // to rerun a seed by hand to learn what one line would have said.
    let why: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for position in 0..jobs.max(1) {
            let (next, found, other, why) = (&next, &found, &other, &why);
            let seeds = &seeds;
            scope.spawn(move || {
                loop {
                    let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(seed) = seeds.get(at) else { break };
                    let bundle = findings.join(format!("seed-{seed}.bohr"));
                    let status = Command::new(me)
                        .args(["run", "--seed", &seed.to_string(), "--config"])
                        .arg(absolute(scenario))
                        .arg("--save")
                        .arg(&bundle)
                        .arg("--")
                        .arg(absolute(binary))
                        .current_dir(root)
                        // A core of its own per sweeping thread, as a campaign gives each worker.
                        .env(CORE_VARIABLE, position.to_string())
                        .output();
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
                                first.get_or_insert_with(|| format!("seed {seed}: {said}"));
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
        why: why.into_inner().ok().flatten(),
    };
    // Sorted, because the threads finish seeds in whatever order the host ran them.
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
        let loaded = bohr_world::config::load(&out).unwrap();
        assert!(!loaded.asks_for_adversity(), "{out}");
        assert!(out.contains("latency_mean_ticks"), "the world stays: {out}");
        assert!(out.contains("snapshot_depth"), "{out}");
        assert!(loaded.node_faults.is_empty());
        assert!(loaded.durability.is_none(), "nothing is killed, so nothing is judged durable");
    }
}
