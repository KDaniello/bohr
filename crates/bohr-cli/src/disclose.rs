//! `bohr report --disclosure` and `bohr report --sarif`: a finding in the form others receive it.
//!
//! The renderers in `bohr-report` open no files. This module reads the bundles, finds the target
//! pack that built a program by its digest, and groups a directory as `bohr findings` does.

use std::path::{Path, PathBuf};

use bohr_report::{RunModel, disclosure::Source, sarif::Finding};
use bohr_trace::Bundle;

use crate::CliError;

fn read(path: &Path) -> Result<Bundle, CliError> {
    let bytes = std::fs::read(path)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot read {}: {e}", path.display())))?;
    Bundle::parse(&bytes).map_err(|e| CliError::Usage(e.to_string()))
}

fn write(out: &str, text: &str) -> Result<(), CliError> {
    std::fs::write(out, text)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot write {out}: {e}")))
}

/// The pack under `targets/` whose build has one of these digests, where one does.
///
/// Matched by digest, since two packs can build programs with the same name.
fn source_of(model: &RunModel) -> Option<Source> {
    let digests: Vec<&str> = model.binaries.iter().map(|record| record.blake3.as_str()).collect();
    crate::targets::pack_of(&digests).map(|pack| Source {
        pack: pack.name,
        repository: pack.repository,
        commit: pack.commit,
    })
}

/// Writes the disclosure draft for one bundle.
pub(crate) fn disclosure(path: &str, out: &str) -> Result<u8, CliError> {
    let bundle = read(Path::new(path))?;
    let model = RunModel::of(&bundle).map_err(|e| CliError::Usage(e.to_string()))?;
    if !bohr_report::narrate::verdict(&model.manifest.outcome).found {
        return Err(CliError::Usage(format!(
            "BOHR-5003: {path} records nothing going wrong, so there is nothing to disclose"
        )));
    }
    let source = source_of(&model);
    write(out, &bohr_report::disclosure::disclosure(&model, path, source.as_ref()))?;
    crate::wrote("disclosure", path, &[out.to_owned()]);
    Ok(crate::exit::CLEAN)
}

/// Writes the SARIF log for one bundle, or for every bundle in a directory, one result per group.
pub(crate) fn sarif(path: &str, out: &str) -> Result<u8, CliError> {
    let given = Path::new(path);
    let mut paths: Vec<PathBuf> = if given.is_dir() {
        std::fs::read_dir(given)
            .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot read {path}: {e}")))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "bohr"))
            .collect()
    } else {
        // A single named bundle must read; a typo is an error.
        RunModel::of(&read(given)?).map_err(|e| CliError::Usage(e.to_string()))?;
        vec![given.to_owned()]
    };
    paths.sort();

    let mut groups: std::collections::BTreeMap<String, Vec<(u64, PathBuf, RunModel)>> =
        std::collections::BTreeMap::new();
    for path in &paths {
        // An unreadable bundle is named and skipped, as in `bohr findings`.
        let read = read(path).and_then(|bundle| {
            let model = RunModel::of(&bundle).map_err(|e| CliError::Usage(e.to_string()))?;
            Ok((bundle, model))
        });
        let (bundle, model) = match read {
            Ok(both) => both,
            Err(e) => {
                eprintln!("unreadable, left out: {}: {e}", path.display());
                continue;
            }
        };
        if !bohr_report::narrate::verdict(&model.manifest.outcome).found {
            continue;
        }
        let signature = crate::findings::signature(&bundle, &model.manifest.outcome, &model.events);
        groups.entry(signature).or_default().push((
            model.manifest.counters.events,
            path.clone(),
            model,
        ));
    }

    let mut findings = Vec::new();
    for (signature, mut members) in groups {
        members.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        let count = members.len();
        let (events, path, model) = members.swap_remove(0);
        let verdict = bohr_report::narrate::verdict(&model.manifest.outcome);
        findings.push(Finding {
            code: verdict.code,
            class: bohr_report::describe::outcome_word(&model.manifest.outcome).to_owned(),
            headline: verdict.headline,
            recorded_by: model.manifest.tool_version.clone(),
            signature,
            location: model.violation.as_ref().and_then(|v| v.location.clone()),
            bundle: path.display().to_string().replace('\\', "/"),
            digest: model.manifest.trace_digest.clone(),
            events,
            members: count,
        });
    }
    // Largest group first, as `bohr findings` prints them.
    findings.sort_by(|a, b| b.members.cmp(&a.members).then_with(|| a.signature.cmp(&b.signature)));

    let log = bohr_report::sarif::sarif(env!("CARGO_PKG_VERSION"), &findings);
    write(out, &serde_json::to_string_pretty(&log).unwrap_or_default())?;
    if crate::json::on() {
        crate::json::emit(
            "report",
            serde_json::json!({
                "kind": "sarif", "bundle": path, "written": [out], "results": findings.len(),
            }),
        );
    } else {
        println!("{out} · {} result{}", findings.len(), if findings.len() == 1 { "" } else { "s" });
    }
    Ok(crate::exit::CLEAN)
}
