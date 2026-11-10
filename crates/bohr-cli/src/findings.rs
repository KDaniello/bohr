//! `bohr findings <dir>`: what a campaign found, grouped by what went wrong.
//!
//! One defect reached from many seeds gives many bundles. Grouped by the violation's signature
//! (the nodes and exit statuses of a crash, the broken promises, the lost files), they read as
//! defects, each led by its smallest bundle.
//!
//! The programs a run executed are part of the signature: two programs exiting 1 are two defects.
//!
//! Two defects with the same signature fall in one group, and one defect with two signatures
//! falls in two.

use std::path::{Path, PathBuf};

use bohr_reducer::Violation;
use bohr_trace::{Bundle, EventKind, Outcome, SectionId};

use crate::CliError;

/// One bundle, as grouping sees it.
struct Read {
    path: PathBuf,
    events: u64,
    signature: String,
    /// What the run found, in words, with its code.
    verdict: bohr_report::narrate::Verdict,
}

/// The signature a bundle's violation is grouped by.
///
/// A hang is named by its words with digits removed, since the manifest lacks the stop reason:
/// "event budget reached at 1000000" and "at 1000001" are the same hang.
pub(crate) fn signature_of(outcome: &Outcome, events: &[bohr_trace::Event]) -> String {
    if let Outcome::GuestHung(said) = outcome {
        let words: String = said.chars().filter(|c| !c.is_ascii_digit()).collect();
        return format!("GuestHung {}", words.split_whitespace().collect::<Vec<_>>().join(" "));
    }
    Violation::of(outcome, bohr_kernel::StopReason::GuestFinished, events).map_or_else(
        || {
            let verdict = bohr_report::narrate::verdict(outcome);
            format!("{}: {}", verdict.code, verdict.headline)
        },
        |v| v.describe(),
    )
}

/// The programs a run executed, by file name, in the order the run started them.
pub(crate) fn programs_of(records: &[bohr_trace::BinaryRecord]) -> String {
    let mut names: Vec<String> = Vec::new();
    for record in records.iter().filter(|r| r.role != "library" && r.role != "loader") {
        let name = record.path.rsplit('/').next().unwrap_or(&record.path).to_owned();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() { "?".to_owned() } else { names.join("+") }
}

/// Whether an event is one a run's group is read from: how tasks exited, what was asserted and
/// diagnosed, and what the world did to the nodes. The rest is the bulk of a log.
pub(crate) const fn telling(event: &bohr_trace::Event) -> bool {
    matches!(
        event.kind,
        EventKind::TaskExited { .. }
            | EventKind::Assertion { .. }
            | EventKind::Diagnostic { .. }
            | EventKind::FaultFired { .. }
            | EventKind::NodeState { .. }
    )
}

/// The group of a run: the programs it ran, its violation's signature, and where it fell.
///
/// The place splits one crash class into its defects: two panics at two frames are two groups. A
/// run that kept no output is grouped by the first two alone.
pub(crate) fn group_of(
    programs: &str,
    outcome: &Outcome,
    events: &[bohr_trace::Event],
    tails: &[bohr_trace::OutputTail],
) -> String {
    let base = format!("{programs}: {}", signature_of(outcome, events));
    let failing = bohr_report::narrate::failing_nodes(events);
    match bohr_report::locate::locate(tails, &failing) {
        Some(place) => format!("{base} @ {}", place.signature()),
        None => base,
    }
}

/// What a bundle is grouped by; see [`group_of`].
pub(crate) fn signature(
    bundle: &Bundle,
    outcome: &Outcome,
    events: &[bohr_trace::Event],
) -> String {
    let records: Vec<bohr_trace::BinaryRecord> = bundle
        .section(SectionId::Binaries)
        .and_then(|raw| serde_json::from_slice(raw).ok())
        .unwrap_or_default();
    let tails: Vec<bohr_trace::OutputTail> = bundle
        .section(SectionId::GuestOutput)
        .and_then(|raw| serde_json::from_slice(raw).ok())
        .unwrap_or_default();
    group_of(&programs_of(&records), outcome, events, &tails)
}

fn read(path: &Path) -> Result<Read, CliError> {
    let bytes = std::fs::read(path)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot read {}: {e}", path.display())))?;
    let bundle = Bundle::parse(&bytes).map_err(|e| CliError::Usage(e.to_string()))?;
    let manifest = bundle.manifest().map_err(|e| CliError::Usage(e.to_string()))?;
    let (events, _) = bundle
        .section(SectionId::Events)
        .map_or_else(|| (Vec::new(), None), bohr_trace::read_events_lossy);
    Ok(Read {
        path: path.to_owned(),
        events: manifest.counters.events,
        signature: signature(&bundle, &manifest.outcome, &events),
        verdict: bohr_report::narrate::verdict(&manifest.outcome),
    })
}

/// Bundles grouped by signature, each group led by its smallest bundle.
type Groups = Vec<(String, Vec<Read>)>;

/// How many bundles `dir` holds, and their groups, largest first.
fn grouped(dir: &str) -> Result<(usize, Groups), CliError> {
    let listed = std::fs::read_dir(dir)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot read {dir}: {e}")))?;
    let mut paths: Vec<PathBuf> = listed
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "bohr"))
        .collect();
    paths.sort();

    let mut groups: std::collections::BTreeMap<String, Vec<Read>> =
        std::collections::BTreeMap::new();
    // An unreadable bundle is named and skipped.
    for path in &paths {
        match read(path) {
            Ok(one) => groups.entry(one.signature.clone()).or_default().push(one),
            Err(e) => eprintln!("unreadable, left out: {}: {e}", path.display()),
        }
    }
    let mut ordered: Vec<(String, Vec<Read>)> = groups.into_iter().collect();
    for (_, members) in &mut ordered {
        members.sort_by(|a, b| (a.events, &a.path).cmp(&(b.events, &b.path)));
    }
    // Largest group first, then by signature, so the same directory prints the same way.
    ordered.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    Ok((paths.len(), ordered))
}

/// The groups as the body of a `bohr.findings` object, for any command that reports a directory.
pub(crate) fn groups_json(dir: &str, ablate: bool) -> Result<serde_json::Value, CliError> {
    let (count, ordered) = grouped(dir)?;
    let judged: Vec<Judged> = ordered.iter().map(|(_, m)| judge(&m[0].path, ablate)).collect();
    Ok(as_json(count, &ordered, &judged, &first_seen(dir)))
}

/// When each group was first reached, as the campaign that wrote `dir` noted it: the run and the
/// seconds since the campaign started. Empty where no campaign wrote it down.
fn first_seen(dir: &str) -> std::collections::BTreeMap<String, (u64, u64)> {
    let text = std::fs::read_to_string(Path::new(dir).join(crate::explore::FIRST_SEEN))
        .unwrap_or_default();
    let listed: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();
    listed
        .iter()
        .filter_map(|entry| {
            Some((
                entry.get("group")?.as_str()?.to_owned(),
                (entry.get("first_run")?.as_u64()?, entry.get("first_after_s")?.as_u64()?),
            ))
        })
        .collect()
}

/// What a group's smallest bundle was judged to be, and the faults it needed where it was ablated.
struct Judged {
    verdict: crate::triage::Verdict,
    necessary: Vec<bohr_trace::FaultKind>,
    grade: Option<bohr_trace::FaultGrade>,
}

/// Triages one bundle, ablating it first where asked. An ablation that cannot run leaves the
/// bundle unjudged, never unreported.
fn judge(path: &Path, ablate: bool) -> Judged {
    let unjudged =
        || Judged { verdict: crate::triage::Verdict::Unjudged, necessary: Vec::new(), grade: None };
    let Ok(bytes) = std::fs::read(path) else { return unjudged() };
    let Ok(bundle) = Bundle::parse(&bytes) else { return unjudged() };
    let ablation = if ablate {
        match crate::ablate::ablation_of(&bundle, None) {
            Ok(ablation) => Some(ablation),
            Err(e) => {
                eprintln!("not ablated: {}: {e}", path.display());
                None
            }
        }
    } else {
        None
    };
    Judged {
        verdict: crate::triage::triage(&bundle, ablation.as_ref()),
        // The kinds it needed, or, where any one of several sufficed, those.
        necessary: ablation
            .as_ref()
            .map(|a| {
                let needed = a.necessary();
                if needed.is_empty() { a.alternatives.clone() } else { needed }
            })
            .unwrap_or_default(),
        grade: ablation.as_ref().and_then(crate::ablate::Ablation::grade),
    }
}

fn as_json(
    count: usize,
    ordered: &[(String, Vec<Read>)],
    judged: &[Judged],
    first: &std::collections::BTreeMap<String, (u64, u64)>,
) -> serde_json::Value {
    let groups: Vec<serde_json::Value> = ordered
        .iter()
        .zip(judged)
        .map(|((signature, members), judged)| {
            serde_json::json!({
                "signature": signature,
                "verdict": members[0].verdict.headline,
                "code": members[0].verdict.code,
                "triage": judged.verdict.name(),
                "counts": judged.verdict.counts(),
                "necessary": judged.necessary.iter().map(|k| k.name()).collect::<Vec<_>>(),
                "grade": judged.grade.map(bohr_trace::FaultGrade::name),
                "first_run": first.get(signature).map(|f| f.0),
                "first_after_s": first.get(signature).map(|f| f.1),
                "count": members.len(),
                "smallest": members[0].path.display().to_string(),
                "bundles": members.iter().map(|m| serde_json::json!({
                    "path": m.path.display().to_string(),
                    "events": m.events,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let defects = judged.iter().filter(|j| j.verdict.counts()).count();
    serde_json::json!({ "findings": count, "groups": groups, "target_defect_groups": defects })
}

/// Groups the bundles in `dir` and prints the groups, largest first, each with its verdict.
///
/// `ablate` repeats each group's smallest bundle once per kind of fault it landed, to say which
/// it needed (`bohr ablate`); without it a group is triaged as far as its bundle alone allows.
pub(crate) fn findings(dir: &str, ablate: bool) -> Result<u8, CliError> {
    let (count, ordered) = grouped(dir)?;
    let judged: Vec<Judged> = ordered.iter().map(|(_, m)| judge(&m[0].path, ablate)).collect();
    let first = first_seen(dir);
    if crate::json::on() {
        crate::json::emit("findings", as_json(count, &ordered, &judged, &first));
    } else {
        let defects = judged.iter().filter(|j| j.verdict.counts()).count();
        println!(
            "{count} findings in {} groups, {defects} of them a defect of the target",
            ordered.len()
        );
        for ((signature, members), judged) in ordered.iter().zip(&judged) {
            let smallest = &members[0];
            println!(
                "\n{:>4} × {}    {}",
                members.len(),
                smallest.verdict.headline,
                smallest.verdict.code
            );
            println!("       grouped as {signature}");
            let needs = if judged.necessary.is_empty() {
                String::new()
            } else {
                format!(
                    " · needs {} ({})",
                    judged.necessary.iter().map(|k| k.name()).collect::<Vec<_>>().join(" + "),
                    judged.grade.map_or("?", bohr_trace::FaultGrade::name)
                )
            };
            println!("       verdict: {}{needs}", judged.verdict.name());
            if let Some((run, after)) = first.get(signature) {
                println!("       first reached at run {run}, {after} s into the campaign");
            }
            println!(
                "       smallest: {} ({} events) · bohr explain {}",
                smallest.path.display(),
                smallest.events,
                smallest.path.display()
            );
        }
    }
    Ok(crate::exit::CLEAN)
}
