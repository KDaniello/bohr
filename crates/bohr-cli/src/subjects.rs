//! Subjects: what `bohr check` checks, and the checks each one declares.
//!
//! A subject is a declaration. A target pack has a `[check]` table in its `target.toml`; a seeded
//! defect has a `bug.toml` beside its scenarios. Each names checks from one fixed set with their
//! bounds, so a new target or defect is a file. The checks live in [`crate::check`].
//!
//! A scenario is found beside the file that names it. The program is the pack's verified binary,
//! or the first word of the scenario's argv. Runs start in the pack's repository, or in the
//! directory `bohr check` was run from.
//!
//! Bounds are written by the subject's author before the run. An unknown check or parameter is
//! refused.

use std::path::{Path, PathBuf};

use crate::{CliError, hunt::parse_seeds, targets};

/// Checks in report order; whether the program runs at all comes first.
pub(crate) const CHECKS: [&str; 12] = [
    "works",
    "repeats",
    "replays",
    "clean",
    "adversity",
    "finds",
    "never_finds",
    "walls",
    "costs",
    "performs",
    "workers_agree",
    "refuses_changed",
];

/// A number written in a subject file, held in thousandths.
///
/// Parsed from the text, never through floating point, so `0.9` stays exactly 900.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Milli(pub(crate) u64);

impl Milli {
    /// `part` of `whole`, rounded down; nothing of nothing is nothing.
    pub(crate) fn of(part: u64, whole: u64) -> Self {
        if whole == 0 {
            return Self(0);
        }
        Self(u64::try_from(u128::from(part) * 1000 / u128::from(whole)).unwrap_or(u64::MAX))
    }

    /// A decimal as written: `5`, `0.9`, `1.15`. At most three places, and never negative.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let (whole, part) = text.split_once('.').unwrap_or((text, ""));
        if whole.is_empty()
            || part.len() > 3
            || !whole.chars().chain(part.chars()).all(|c| c.is_ascii_digit())
        {
            return None;
        }
        let whole: u64 = whole.parse().ok()?;
        let part: u64 = if part.is_empty() { 0 } else { format!("{part:0<3}").parse().ok()? };
        whole.checked_mul(1000)?.checked_add(part).map(Self)
    }

    /// As a share, in whole percent.
    pub(crate) fn percent(self) -> u64 {
        self.0 / 10
    }
}

impl std::fmt::Display for Milli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (whole, part) = (self.0 / 1000, self.0 % 1000);
        if part == 0 {
            write!(f, "{whole}")
        } else {
            let part = format!("{part:03}");
            write!(f, "{whole}.{}", part.trim_end_matches('0'))
        }
    }
}

/// What the cost of a scenario is measured by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Metric {
    /// Events recorded: the same on every host, so a bound on it holds anywhere.
    Events,
    /// Wall-clock time: what a person waits, and only comparable on one host.
    Wall,
}

/// One check, with its parameters read and its defaults applied.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Kind {
    Works { seed: u64, min_events: u64, min_virtual_s: Milli, probe_answers: Option<Milli> },
    Repeats { seed: u64, times: u64 },
    Replays { seed: u64 },
    Clean { seeds: (u64, u64) },
    Adversity { seeds: (u64, u64), at_least: Milli },
    Finds { seeds: (u64, u64), code: String, at_least: u64 },
    NeverFinds { seeds: (u64, u64) },
    Walls { seed: u64, allow: Vec<String> },
    Costs { seed: u64, against: PathBuf, metric: Metric, max_ratio: Milli },
    Performs(Performs),
    WorkersAgree { runs: u64, jobs: usize },
    RefusesChanged,
}

/// What `performs` measures a golden workload against: its own run with no operations, and the
/// same program run natively.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Performs {
    /// Operations the workload does.
    pub(crate) ops: u64,
    /// Attempts per side, the fastest kept.
    pub(crate) best_of: u64,
    /// The scenario with no operations: everything the run costs that is not the workload.
    pub(crate) zero: String,
    /// The workload's argv, and the same with no operations, to run natively; `None` for a
    /// workload of several nodes, which has no native form.
    pub(crate) native: Option<(Vec<String>, Vec<String>)>,
    pub(crate) max_us_per_op: Option<Milli>,
    pub(crate) max_slowdown: Option<Milli>,
    pub(crate) max_rss_mib: Option<u64>,
    pub(crate) max_stops_per_op: Option<Milli>,
}

/// A check as a subject declared it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Check {
    /// Its name in the fixed set.
    pub(crate) name: &'static str,
    /// The scenario it runs.
    pub(crate) scenario: PathBuf,
    /// The scenario's text, where the subject generates it.
    pub(crate) generated: Option<String>,
    pub(crate) kind: Kind,
}

/// Checks that share a label: a subject's own, or one defect's.
#[derive(Clone, Debug)]
pub(crate) struct Group {
    /// The defect or the workload they are about, where they are about one.
    pub(crate) label: Option<String>,
    /// What the label stands for, in a line: for a golden workload, the parts of bohr it loads.
    pub(crate) note: Option<String>,
    pub(crate) checks: Vec<Check>,
}

/// A subject, resolved.
#[derive(Clone, Debug)]
pub(crate) struct Subject {
    /// Its name: the pack's, or the directory its `bug.toml` sits in.
    pub(crate) name: String,
    /// One line a reader identifies it by.
    pub(crate) about: String,
    /// Where runs start.
    pub(crate) cwd: PathBuf,
    /// The pack, where the subject is one.
    pub(crate) pack: Option<targets::Pack>,
    pub(crate) groups: Vec<Group>,
}

fn refused(file: &Path, text: impl std::fmt::Display) -> CliError {
    CliError::refused(format!("{}: {text}", file.display()))
}

/// Finds a subject by name, or by the path of its directory or its file.
///
/// A bare name is a pack under `targets/` where there is one, then a defect under `testdata/bugs/`,
/// then an example under `examples/`; `golden` is the golden guests.
pub(crate) fn resolve(name: &str) -> Result<Subject, CliError> {
    let given = Path::new(name);
    let bare = given.components().count() == 1 && !name.ends_with(".toml");
    let file = if bare && name == "golden" {
        Path::new("testdata/golden/golden.toml").to_owned()
    } else if bare {
        let candidates = [
            Path::new("targets").join(name).join("target.toml"),
            Path::new("testdata/bugs").join(name).join("bug.toml"),
            Path::new("examples").join(name).join("bug.toml"),
        ];
        match candidates.iter().find(|c| c.is_file()) {
            Some(found) => found.clone(),
            None => {
                return Err(CliError::usage(format!(
                    "BOHR-1002: no subject named {name}: looked for {}",
                    candidates
                        .iter()
                        .map(|c| c.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
    } else if given.is_dir() {
        if given.join("target.toml").is_file() {
            given.join("target.toml")
        } else {
            given.join("bug.toml")
        }
    } else {
        given.to_owned()
    };
    if file.file_name().is_some_and(|f| f == "target.toml") {
        pack_subject(&file)
    } else if file.file_name().is_some_and(|f| f == "golden.toml") {
        golden_subject(&file)
    } else {
        bug_subject(&file)
    }
}

/// Every subject this repository declares, packs first.
pub(crate) fn all() -> Result<Vec<Subject>, CliError> {
    let mut files = Vec::new();
    for (dir, file) in
        [("targets", "target.toml"), ("testdata/bugs", "bug.toml"), ("examples", "bug.toml")]
    {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        let mut found: Vec<PathBuf> =
            entries.flatten().map(|e| e.path().join(file)).filter(|p| p.is_file()).collect();
        found.sort();
        files.extend(found);
    }
    let golden = Path::new("testdata/golden/golden.toml");
    if golden.is_file() {
        files.push(golden.to_owned());
    }
    files.iter().map(|f| resolve(&f.to_string_lossy())).collect()
}

fn pack_subject(file: &Path) -> Result<Subject, CliError> {
    let pack = targets::resolve(&file.to_string_lossy())?;
    let dir = pack.root.join("targets").join(&pack.name);
    let checks = match &pack.checks {
        Some(table) => read_checks(table, &dir, &pack.scenario, file, true)?,
        None => Vec::new(),
    };
    let digest = &pack.digest;
    let short = if digest.len() > 12 {
        format!("{}…{}", &digest[..8], &digest[digest.len() - 4..])
    } else {
        digest.clone()
    };
    Ok(Subject {
        name: pack.name.clone(),
        about: format!(
            "targets/{} · {} at {} · blake3 {short}",
            pack.name, pack.repository, pack.commit
        ),
        cwd: pack.root.clone(),
        groups: vec![Group { label: None, note: None, checks }],
        pack: Some(pack),
    })
}

fn bug_subject(file: &Path) -> Result<Subject, CliError> {
    let text =
        std::fs::read_to_string(file).map_err(|e| refused(file, format!("cannot read it: {e}")))?;
    let table: toml::Table = toml::from_str(&text).map_err(|e| refused(file, e))?;
    let file = std::path::absolute(file).unwrap_or_else(|_| file.to_owned());
    let dir = file.parent().unwrap_or(Path::new(".")).to_owned();
    let name = dir.file_name().map_or_else(|| "subject".to_owned(), |n| n.to_string_lossy().into());

    let mut about = String::new();
    let mut scenario = dir.join("scenario.toml");
    let mut groups = Vec::new();
    let mut defects = Vec::new();
    for (key, value) in &table {
        match (key.as_str(), value) {
            ("defect", toml::Value::String(text)) => about.clone_from(text),
            ("defect", toml::Value::Array(items)) => defects.extend(items.iter().cloned()),
            ("scenario", toml::Value::String(path)) => scenario = dir.join(path),
            ("check", _) => {}
            (other, _) => {
                return Err(refused(
                    &file,
                    format!("{other:?} is not a key of a bug.toml: defect, scenario, check"),
                ));
            }
        }
    }
    if let Some(checks) = table.get("check") {
        let toml::Value::Table(checks) = checks else {
            return Err(refused(&file, "[check] is a table"));
        };
        groups.push(Group {
            label: None,
            note: None,
            checks: read_checks(checks, &dir, &scenario, &file, false)?,
        });
    }
    for defect in defects {
        let toml::Value::Table(defect) = defect else {
            return Err(refused(&file, "each [[defect]] is a table"));
        };
        let mut label = None;
        let mut checks = Vec::new();
        for (key, value) in &defect {
            match (key.as_str(), value) {
                ("name", toml::Value::String(text)) => label = Some(text.clone()),
                ("check", toml::Value::Table(table)) => {
                    checks = read_checks(table, &dir, &scenario, &file, false)?;
                }
                (other, _) => {
                    return Err(refused(
                        &file,
                        format!("{other:?} is not a key of a [[defect]]: name, check"),
                    ));
                }
            }
        }
        let Some(label) = label else {
            return Err(refused(&file, "a [[defect]] needs a name"));
        };
        groups.push(Group { label: Some(label), note: None, checks });
    }
    Ok(Subject {
        about: match (about.is_empty(), groups.iter().filter(|g| g.label.is_some()).count()) {
            (false, _) => about,
            (true, 0) => name.clone(),
            (true, n) => format!("{n} seeded defects, and the program without them"),
        },
        name,
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        pack: None,
        groups,
    })
}

/// One golden workload, as the scenarios it generates.
struct Workload {
    /// The scenario that runs it.
    text: String,
    /// The same with no operations.
    zero: String,
    native: Option<(Vec<String>, Vec<String>)>,
    ops: u64,
}

/// Virtual time a golden workload may take; far more than any needs, so only a hang reaches it.
const GOLDEN_HORIZON_TICKS: i64 = 600_000_000_000;

/// The scenario a golden workload runs in: `nodes` nodes, each running the golden program with the
/// workload's name and count, and whatever the workload adds — a disk or a network that fails.
fn workload_scenario(
    program: &str,
    workload: &str,
    count: u64,
    nodes: u64,
    extra: &toml::Table,
) -> String {
    let mut argv = vec![program.to_owned(), workload.to_owned(), count.to_string()];
    if nodes > 1 {
        argv.push("{node}".to_owned());
    }
    let mut scenario = toml::Table::new();
    let mut run = toml::Table::new();
    run.insert("horizon_ticks".into(), toml::Value::Integer(GOLDEN_HORIZON_TICKS));
    scenario.insert("run".into(), toml::Value::Table(run));
    let mut cluster = toml::Table::new();
    cluster.insert("nodes".into(), toml::Value::Integer(i64::try_from(nodes).unwrap_or(1)));
    cluster.insert(
        "argv".into(),
        toml::Value::Array(argv.into_iter().map(toml::Value::String).collect()),
    );
    scenario.insert("cluster".into(), toml::Value::Table(cluster));
    for (key, value) in extra {
        match (scenario.get_mut(key), value) {
            (Some(toml::Value::Table(have)), toml::Value::Table(add)) => {
                have.extend(add.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            _ => {
                scenario.insert(key.clone(), value.clone());
            }
        }
    }
    toml::to_string(&scenario).unwrap_or_default()
}

/// Reads one whole-number key of a table, where it is one and not negative.
fn count_in(table: &toml::Table, key: &str) -> Option<u64> {
    table.get(key).and_then(toml::Value::as_integer).and_then(|n| u64::try_from(n).ok())
}

/// The golden guests: one workload per part of bohr, each held to the same checks.
fn golden_subject(file: &Path) -> Result<Subject, CliError> {
    let text =
        std::fs::read_to_string(file).map_err(|e| refused(file, format!("cannot read it: {e}")))?;
    let table: toml::Table = toml::from_str(&text).map_err(|e| refused(file, e))?;
    let file = std::path::absolute(file).unwrap_or_else(|_| file.to_owned());
    let dir = file.parent().unwrap_or(Path::new(".")).to_owned();
    for key in table.keys() {
        if !["about", "program", "workload"].contains(&key.as_str()) {
            return Err(refused(
                &file,
                format!("{key:?} is not a key of golden.toml: about, program, workload"),
            ));
        }
    }
    let program = table
        .get("program")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| refused(&file, "golden.toml names its program"))?
        .to_owned();
    let about = table.get("about").and_then(toml::Value::as_str).unwrap_or("golden guests");
    let empty = Vec::new();
    let workloads = match table.get("workload") {
        Some(toml::Value::Array(items)) => items,
        None => &empty,
        Some(_) => return Err(refused(&file, "each [[workload]] is a table")),
    };
    let mut groups = Vec::new();
    for item in workloads {
        let toml::Value::Table(item) = item else {
            return Err(refused(&file, "each [[workload]] is a table"));
        };
        let keys = ["name", "workload", "count", "nodes", "module", "scenario", "check"];
        if let Some(key) = item.keys().find(|k| !keys.contains(&k.as_str())) {
            return Err(refused(
                &file,
                format!("{key:?} is not a key of a [[workload]]: {}", keys.join(", ")),
            ));
        }
        let name = item
            .get("name")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| refused(&file, "a [[workload]] needs a name"))?;
        let workload = item.get("workload").and_then(toml::Value::as_str).unwrap_or(name);
        let count = count_in(item, "count")
            .ok_or_else(|| refused(&file, format!("workload {name} needs a count")))?;
        let nodes = count_in(item, "nodes").unwrap_or(1).max(1);
        let extra = match item.get("scenario") {
            Some(toml::Value::Table(extra)) => extra.clone(),
            None => toml::Table::new(),
            Some(_) => return Err(refused(&file, format!("workload {name}: scenario is a table"))),
        };
        // Natively only where there is a native form: one process, nothing done to it.
        let native = (nodes == 1 && extra.is_empty()).then(|| {
            let argv = |n: u64| vec![program.clone(), workload.to_owned(), n.to_string()];
            (argv(count), argv(0))
        });
        let generated = Workload {
            text: workload_scenario(&program, workload, count, nodes, &extra),
            zero: workload_scenario(&program, workload, 0, nodes, &extra),
            native,
            ops: count,
        };
        let checks = match item.get("check") {
            Some(toml::Value::Table(checks)) => read_checks_for(
                checks,
                &dir,
                &dir.join(format!("{name}.toml")),
                &file,
                false,
                Some(&generated),
            )?,
            None => Vec::new(),
            Some(_) => return Err(refused(&file, format!("workload {name}: check is a table"))),
        };
        groups.push(Group {
            label: Some(name.to_owned()),
            note: item.get("module").and_then(toml::Value::as_str).map(str::to_owned),
            checks,
        });
    }
    Ok(Subject {
        name: "golden".to_owned(),
        about: about.to_owned(),
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        pack: None,
        groups,
    })
}

/// Reads a `[check]` table, in the fixed order whatever order it was written in.
fn read_checks(
    table: &toml::Table,
    dir: &Path,
    default_scenario: &Path,
    file: &Path,
    is_pack: bool,
) -> Result<Vec<Check>, CliError> {
    read_checks_for(table, dir, default_scenario, file, is_pack, None)
}

/// Reads a `[check]` table; a golden workload's checks run the scenario it generates.
fn read_checks_for(
    table: &toml::Table,
    dir: &Path,
    default_scenario: &Path,
    file: &Path,
    is_pack: bool,
    workload: Option<&Workload>,
) -> Result<Vec<Check>, CliError> {
    for key in table.keys() {
        if !CHECKS.contains(&key.as_str()) {
            return Err(refused(
                file,
                format!("no check named {key:?}; the checks are {}", CHECKS.join(", ")),
            ));
        }
    }
    let mut out = Vec::new();
    for name in CHECKS {
        let Some(value) = table.get(name) else { continue };
        let empty = toml::Table::new();
        let params = match value {
            toml::Value::Table(params) => params,
            toml::Value::Boolean(true) => &empty,
            _ => return Err(refused(file, format!("check {name} takes a table of parameters"))),
        };
        let mut p = Params { name, table: params, used: Vec::new(), file };
        let scenario =
            p.string("scenario")?.map_or_else(|| default_scenario.to_owned(), |s| dir.join(s));
        let kind = match name {
            "works" => Kind::Works {
                seed: p.int("seed")?.unwrap_or(1),
                min_events: p.int("min_events")?.unwrap_or(1),
                min_virtual_s: p.float("min_virtual_s")?.unwrap_or(Milli(0)),
                probe_answers: p.fraction("probe_answers")?,
            },
            "repeats" => {
                Kind::Repeats { seed: p.int("seed")?.unwrap_or(1), times: p.required_int("times")? }
            }
            "replays" => Kind::Replays { seed: p.int("seed")?.unwrap_or(1) },
            "clean" => Kind::Clean { seeds: p.seeds()? },
            "adversity" => Kind::Adversity {
                seeds: p.seeds()?,
                at_least: p.fraction("at_least")?.ok_or_else(|| p.missing("at_least"))?,
            },
            "finds" => Kind::Finds {
                seeds: p.seeds()?,
                code: p.string("code")?.ok_or_else(|| p.missing("code"))?,
                at_least: p.required_int("at_least")?,
            },
            "never_finds" => Kind::NeverFinds { seeds: p.seeds()? },
            "walls" => {
                Kind::Walls { seed: p.int("seed")?.unwrap_or(1), allow: p.strings("allow")? }
            }
            "costs" => Kind::Costs {
                seed: p.int("seed")?.unwrap_or(1),
                against: dir.join(p.string("against")?.ok_or_else(|| p.missing("against"))?),
                metric: match p.string("metric")?.as_deref() {
                    None | Some("wall") => Metric::Wall,
                    Some("events") => Metric::Events,
                    Some(other) => {
                        return Err(refused(
                            file,
                            format!("costs.metric {other:?} is wall or events"),
                        ));
                    }
                },
                max_ratio: p.float("max_ratio")?.ok_or_else(|| p.missing("max_ratio"))?,
            },
            "workers_agree" => Kind::WorkersAgree {
                runs: p.required_int("runs")?,
                jobs: usize::try_from(p.int("jobs")?.unwrap_or(8)).unwrap_or(8),
            },
            "performs" => {
                let Some(workload) = workload else {
                    return Err(refused(
                        file,
                        "performs is a golden workload's check: only a workload knows its operations",
                    ));
                };
                Kind::Performs(Performs {
                    ops: workload.ops,
                    best_of: p.int("best_of")?.unwrap_or(3).max(1),
                    zero: workload.zero.clone(),
                    native: workload.native.clone(),
                    max_us_per_op: p.float("max_us_per_op")?,
                    max_slowdown: p.float("max_slowdown")?,
                    max_rss_mib: p.int("max_rss_mib")?,
                    max_stops_per_op: p.float("max_stops_per_op")?,
                })
            }
            "refuses_changed" => {
                if !is_pack {
                    return Err(refused(
                        file,
                        "refuses_changed is a pack's check: only a pack pins a binary",
                    ));
                }
                Kind::RefusesChanged
            }
            _ => unreachable!("every name in CHECKS is matched"),
        };
        p.finish()?;
        if workload.is_some() && params.contains_key("scenario") {
            return Err(refused(file, "a golden workload's checks run the scenario it generates"));
        }
        out.push(Check { name, scenario, generated: workload.map(|w| w.text.clone()), kind });
    }
    Ok(out)
}

/// One check's parameters, read once each; whatever is left unread is a mistake.
struct Params<'a> {
    name: &'static str,
    table: &'a toml::Table,
    used: Vec<&'static str>,
    file: &'a Path,
}

impl Params<'_> {
    fn get(&mut self, key: &'static str) -> Option<&toml::Value> {
        self.used.push(key);
        self.table.get(key)
    }

    fn wrong(&self, key: &str, want: &str) -> CliError {
        refused(self.file, format!("{}.{key} is {want}", self.name))
    }

    fn missing(&self, key: &str) -> CliError {
        refused(self.file, format!("check {} needs {key}", self.name))
    }

    fn int(&mut self, key: &'static str) -> Result<Option<u64>, CliError> {
        match self.get(key) {
            None => Ok(None),
            Some(toml::Value::Integer(n)) if *n >= 0 => Ok(Some(n.unsigned_abs())),
            Some(_) => Err(self.wrong(key, "a whole number, not negative")),
        }
    }

    fn required_int(&mut self, key: &'static str) -> Result<u64, CliError> {
        self.int(key)?.ok_or_else(|| self.missing(key))
    }

    fn float(&mut self, key: &'static str) -> Result<Option<Milli>, CliError> {
        let text = match self.get(key) {
            None => return Ok(None),
            Some(toml::Value::Float(x)) => x.to_string(),
            Some(toml::Value::Integer(n)) => n.to_string(),
            Some(_) => String::new(),
        };
        Milli::parse(&text)
            .map(Some)
            .ok_or_else(|| self.wrong(key, "a number, not negative, with at most three decimals"))
    }

    fn fraction(&mut self, key: &'static str) -> Result<Option<Milli>, CliError> {
        match self.float(key)? {
            Some(x) if x > Milli(1000) => Err(self.wrong(key, "a fraction between 0 and 1")),
            other => Ok(other),
        }
    }

    fn string(&mut self, key: &'static str) -> Result<Option<String>, CliError> {
        match self.get(key) {
            None => Ok(None),
            Some(toml::Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(self.wrong(key, "a string")),
        }
    }

    fn strings(&mut self, key: &'static str) -> Result<Vec<String>, CliError> {
        let read: Option<Vec<String>> = match self.get(key) {
            None => return Ok(Vec::new()),
            Some(toml::Value::Array(items)) => {
                items.iter().map(|v| v.as_str().map(str::to_owned)).collect()
            }
            Some(_) => None,
        };
        read.ok_or_else(|| self.wrong(key, "a list of strings"))
    }

    fn seeds(&mut self) -> Result<(u64, u64), CliError> {
        let text = self.string("seeds")?.ok_or_else(|| self.missing("seeds"))?;
        parse_seeds(&text).map_err(|e| self.wrong("seeds", &format!("a range a..b: {e}")))
    }

    fn finish(self) -> Result<(), CliError> {
        let unknown: Vec<&String> =
            self.table.keys().filter(|k| !self.used.contains(&k.as_str())).collect();
        match unknown.first() {
            None => Ok(()),
            Some(key) => {
                Err(refused(self.file, format!("check {} has no parameter {key:?}", self.name)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checks(text: &str, is_pack: bool) -> Result<Vec<Check>, CliError> {
        let table: toml::Table = toml::from_str(text).unwrap();
        read_checks(
            &table,
            Path::new("/s"),
            Path::new("/s/scenario.toml"),
            Path::new("/s/bug.toml"),
            is_pack,
        )
    }

    /// Checks come back in the fixed order, with defaults applied and scenarios found beside the
    /// file that names them.
    #[test]
    fn a_check_table_reads_in_the_fixed_order() {
        let read = checks(
            r#"
never_finds = { scenario = "ample.toml", seeds = "1..60" }
repeats = { times = 100 }
finds = { seeds = "1..60", code = "GuestCrashed", at_least = 20 }
"#,
            false,
        )
        .unwrap();
        let names: Vec<&str> = read.iter().map(|c| c.name).collect();
        assert_eq!(names, ["repeats", "finds", "never_finds"]);
        assert_eq!(read[0].kind, Kind::Repeats { seed: 1, times: 100 });
        assert_eq!(read[0].scenario, Path::new("/s/scenario.toml"));
        assert_eq!(read[2].scenario, Path::new("/s/ample.toml"));
        assert_eq!(
            read[1].kind,
            Kind::Finds { seeds: (1, 60), code: "GuestCrashed".into(), at_least: 20 }
        );
    }

    /// A key no check reads is refused, so a misspelt bound cannot pass silently.
    #[test]
    fn an_unknown_check_or_parameter_is_refused() {
        let err = |text: &str| checks(text, false).unwrap_err().to_string();
        assert!(err("finds_it = true").contains("no check named"));
        assert!(err("repeats = { times = 10, time = 3 }").contains("no parameter \"time\""));
        assert!(err("finds = { seeds = \"1..6\", code = \"X\" }").contains("needs at_least"));
        assert!(err("adversity = { seeds = \"1..6\", at_least = 90 }").contains("fraction"));
        assert!(err("refuses_changed = true").contains("pack's check"));
        assert!(checks("refuses_changed = true", true).is_ok());
        assert!(
            err("costs = { against = \"a.toml\", max_ratio = 1.1234 }").contains("three decimals")
        );
    }

    /// A bound reads exactly as written, and a share is compared in whole thousandths.
    #[test]
    fn a_bound_is_read_from_its_text() {
        assert_eq!(Milli::parse("0.9"), Some(Milli(900)));
        assert_eq!(Milli::parse("1.15"), Some(Milli(1150)));
        assert_eq!(Milli::parse("5"), Some(Milli(5000)));
        assert_eq!(Milli::parse("-1"), None);
        assert_eq!(Milli::parse("1e3"), None);
        assert_eq!(Milli(1150).to_string(), "1.15");
        assert_eq!(Milli(5000).to_string(), "5");
        assert!(Milli::of(9, 10) >= Milli::parse("0.9").unwrap(), "nine of ten is ninety percent");
        assert!(Milli::of(89, 100) < Milli::parse("0.9").unwrap());
        assert_eq!(Milli::of(3, 0), Milli(0));
    }
}
