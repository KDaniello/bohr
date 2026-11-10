//! `bohr findings <dir>`: what a campaign found, grouped by what went wrong rather than by run.
//!
//! A campaign against a real client writes one bundle per distinct run that went wrong, and one
//! defect reached from many seeds is many bundles. Grouped by the violation's signature — the
//! nodes and exit statuses of a crash, the promises a property run broke, the files a durability
//! failure lost — they read as defects, each led by its smallest bundle, which is the one to reduce
//! and the one to read.
//!
//! The programs a run executed are part of the signature: two programs that both exit 1 are two
//! defects, and nothing in an exit status says which program it came from.
//!
//! What this does not do: tell two defects of one program apart that leave the same signature, or
//! join one defect that leaves two. The signature is what a reduction already treats as "the same
//! finding".

use std::path::{Path, PathBuf};

use bohr_reducer::Violation;
use bohr_trace::{Bundle, Outcome, SectionId};

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
/// A hang is named by its words with the numbers taken out, because the stop reason the
/// signature would carry is not in the manifest and the words say it: "event budget reached at
/// 1000000" and "at 1000001" are one way of not finishing.
fn signature_of(outcome: &Outcome, events: &[bohr_trace::Event]) -> String {
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
fn programs_of(bundle: &Bundle) -> String {
    let records: Vec<bohr_trace::BinaryRecord> = bundle
        .section(SectionId::Binaries)
        .and_then(|raw| serde_json::from_slice(raw).ok())
        .unwrap_or_default();
    let mut names: Vec<String> = Vec::new();
    for record in records.iter().filter(|r| r.role != "library" && r.role != "loader") {
        let name = record.path.rsplit('/').next().unwrap_or(&record.path).to_owned();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() { "?".to_owned() } else { names.join("+") }
}

/// What a bundle is grouped by: the programs it ran and its violation's signature.
pub(crate) fn signature(
    bundle: &Bundle,
    outcome: &Outcome,
    events: &[bohr_trace::Event],
) -> String {
    format!("{}: {}", programs_of(bundle), signature_of(outcome, events))
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

/// Groups the bundles in `dir` and prints the groups, largest first.
pub(crate) fn findings(dir: &str, json: bool) -> Result<u8, CliError> {
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
    // A bundle that does not read is named and left out, not allowed to hide every other group.
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

    if json {
        let groups: Vec<serde_json::Value> = ordered
            .iter()
            .map(|(signature, members)| {
                serde_json::json!({
                    "signature": signature,
                    "verdict": members[0].verdict.headline,
                    "code": members[0].verdict.code,
                    "count": members.len(),
                    "smallest": members[0].path.display().to_string(),
                    "bundles": members.iter().map(|m| serde_json::json!({
                        "path": m.path.display().to_string(),
                        "events": m.events,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        let account = serde_json::json!({ "findings": paths.len(), "groups": groups });
        println!("{}", serde_json::to_string_pretty(&account).unwrap_or_default());
    } else {
        println!("{} findings in {} groups", paths.len(), ordered.len());
        for (signature, members) in &ordered {
            let smallest = &members[0];
            println!(
                "\n{:>4} × {}    {}",
                members.len(),
                smallest.verdict.headline,
                smallest.verdict.code
            );
            println!("       grouped as {signature}");
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
