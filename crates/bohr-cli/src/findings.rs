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

use bohr_trace::{Bundle, Outcome};

use crate::CliError;

/// One bundle, as grouping sees it.
struct Read {
    path: PathBuf,
    /// Whether the run it holds went clean, which no finding does.
    clean: bool,
    events: u64,
    signature: String,
    /// What the run found, in words, with its code.
    verdict: bohr_report::narrate::Verdict,
    /// What the bundle alone says the finding is, before any ablation.
    triaged: bohr_findings::triage::Verdict,
}

fn read(path: &Path) -> Result<Read, CliError> {
    let bytes = std::fs::read(path)
        .map_err(|e| CliError::usage(format!("BOHR-1002: cannot read {}: {e}", path.display())))?;
    let bundle = Bundle::parse(&bytes).map_err(|e| CliError::usage(e.to_string()))?;
    let manifest = bundle.manifest().map_err(|e| CliError::usage(e.to_string()))?;
    // Only the events a group and a verdict are read from are kept, as a campaign keeps them when
    // it names the group: the rest is the bulk of the log.
    let events = bohr_findings::group::telling_events(&bundle)
        .map_err(|e| CliError::usage(e.to_string()))?;
    Ok(Read {
        path: path.to_owned(),
        clean: matches!(manifest.outcome, Outcome::Clean),
        events: manifest.counters.events,
        signature: bohr_findings::group::signature(&bundle, &manifest.outcome, &events)
            .map_err(|e| CliError::usage(e.to_string()))?,
        verdict: bohr_report::narrate::verdict(&manifest.outcome),
        triaged: bohr_findings::triage::triage(&bundle, &events, None, &nonproduction_of(&bundle)),
    })
}

/// Bundles grouped by signature, each group led by its smallest bundle.
type Groups = Vec<(String, Vec<Read>)>;

/// How many bundles `dir` holds, and their groups, largest first.
fn grouped(dir: &str) -> Result<(usize, Groups), CliError> {
    let listed = std::fs::read_dir(dir)
        .map_err(|e| CliError::usage(format!("BOHR-1002: cannot read {dir}: {e}")))?;
    let mut paths: Vec<PathBuf> = listed
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "bohr"))
        // A re-run that did not reproduce its finding is the tool's failure, kept for whoever
        // looks into it and never counted.
        .filter(|path| {
            !path.file_name().is_some_and(|name| name.to_string_lossy().starts_with(UNREPRODUCED))
        })
        .collect();
    paths.sort();

    let mut groups: std::collections::BTreeMap<String, Vec<Read>> =
        std::collections::BTreeMap::new();
    let mut counted = 0;
    // An unreadable bundle is named and skipped; a clean one is no finding and is named apart.
    for path in &paths {
        match read(path) {
            Ok(one) if one.clean => eprintln!("clean, not a finding: {}", path.display()),
            Ok(one) => {
                counted += 1;
                groups.entry(one.signature.clone()).or_default().push(one);
            }
            Err(e) => eprintln!("unreadable, left out: {}: {e}", path.display()),
        }
    }
    let mut ordered: Vec<(String, Vec<Read>)> = groups.into_iter().collect();
    for (_, members) in &mut ordered {
        members.sort_by(|a, b| (a.events, &a.path).cmp(&(b.events, &b.path)));
    }
    // Largest group first, then by signature, so the same directory prints the same way.
    ordered.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    Ok((counted, ordered))
}

/// The name a finding's re-run is kept under when it did not reproduce the finding.
pub(crate) const UNREPRODUCED: &str = "unreproduced-";

/// The groups as the body of a `bohr.findings` object, for any command that reports a directory.
pub(crate) fn groups_json(dir: &str, ablate: bool) -> Result<serde_json::Value, CliError> {
    let (count, ordered) = grouped(dir)?;
    let judged: Vec<Judged> = ordered.iter().map(|(_, m)| judge(&m[0], ablate)).collect();
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
    verdict: bohr_findings::triage::Verdict,
    necessary: Vec<bohr_trace::FaultKind>,
    grade: Option<bohr_trace::FaultGrade>,
}

/// Triages a group's smallest bundle, ablating it first where asked. An ablation that cannot run
/// leaves the bundle judged as far as it alone allows, never unreported.
///
/// Without ablation the verdict is the one reading the bundle gave; with it the bundle is read
/// again, which costs nothing beside the trials an ablation runs.
fn judge(head: &Read, ablate: bool) -> Judged {
    let alone = || Judged { verdict: head.triaged.clone(), necessary: Vec::new(), grade: None };
    if !ablate {
        return alone();
    }
    let Ok(bytes) = std::fs::read(&head.path) else { return alone() };
    let Ok(bundle) = Bundle::parse(&bytes) else { return alone() };
    let Ok(events) = bohr_findings::group::telling_events(&bundle) else { return alone() };
    let ablation = match crate::ablate::ablation_of(&bundle, None) {
        Ok(ablation) => ablation,
        Err(e) => {
            eprintln!("not ablated: {}: {e}", head.path.display());
            return alone();
        }
    };
    // The kinds it needed, or, where any one of several sufficed, those.
    let needed = ablation.necessary();
    Judged {
        verdict: bohr_findings::triage::triage(
            &bundle,
            &events,
            Some(&ablation),
            &nonproduction_of(&bundle),
        ),
        necessary: if needed.is_empty() { ablation.alternatives.clone() } else { needed },
        grade: ablation.grade(),
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
    let judged: Vec<Judged> = ordered.iter().map(|(_, m)| judge(&m[0], ablate)).collect();
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
            println!("       grouped as {}", bohr_report::text::printable(signature));
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

/// The words the pack of a bundle's programs declares non-production; none where they belong to no
/// installed pack, or the list of programs does not read.
fn nonproduction_of(bundle: &Bundle) -> Vec<String> {
    let binaries = bundle.binaries().unwrap_or_default();
    let digests: Vec<&str> = binaries.iter().map(|b| b.blake3.as_str()).collect();
    crate::targets::pack_of(&digests).map(|pack| pack.nonproduction).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use bohr_trace::SectionId;

    use super::*;

    /// A finding whose output section does not read is left out as unreadable, not grouped as a
    /// run that printed nothing.
    #[test]
    fn a_bundle_whose_output_does_not_read_is_not_grouped() {
        let golden = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/bundles/two-tasks-seed0.bohr");
        let bundle =
            Bundle::parse(&std::fs::read(golden).expect("the golden corpus")).expect("parse");
        let mut manifest = bundle.manifest().expect("manifest");
        manifest.outcome = Outcome::GuestCrashed("killed by signal 11".to_owned());
        let mut writer = bundle.to_writer();
        writer.section(SectionId::Manifest, serde_json::to_vec(&manifest).expect("json"));
        writer.section(SectionId::GuestOutput, b"not json".to_vec());
        let dir =
            std::env::temp_dir().join(format!("bohr-unreadable-tails-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        std::fs::write(dir.join("finding-000000000000.bohr"), writer.finish(bundle.run_id))
            .expect("written");
        let found = groups_json(&dir.to_string_lossy(), false).expect("read");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found["findings"], 0, "{found}");
    }

    /// A clean bundle, and a re-run that did not reproduce its finding, are no findings.
    #[test]
    fn a_clean_or_unreproduced_bundle_is_no_finding() {
        let golden = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/bundles/clock-and-random-seed0.bohr");
        let bytes = std::fs::read(&golden).expect("the golden corpus");
        let dir = std::env::temp_dir().join(format!("bohr-clean-findings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        std::fs::write(dir.join("finding-000000000000.bohr"), &bytes).expect("written");
        std::fs::write(dir.join(format!("{UNREPRODUCED}111111111111.bohr")), &bytes)
            .expect("written");
        let found = groups_json(&dir.to_string_lossy(), false).expect("read");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found["findings"], 0, "{found}");
        assert_eq!(found["groups"].as_array().map(Vec::len), Some(0), "{found}");
        assert!(
            crate::ablate::ablation(&bytes, None)
                .is_err_and(|e| e.to_string().contains("BOHR-5003")),
            "a clean bundle was ablated"
        );
    }
}
