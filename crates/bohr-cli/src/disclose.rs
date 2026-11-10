//! `bohr report --disclosure` and `bohr report --sarif`: a finding as somebody else receives it.
//!
//! The renderers are in `bohr-report` and open nothing. What is here is what they cannot do:
//! read the bundles, find the target pack that built a program by its digest, and group a
//! directory of findings the way `bohr findings` does.

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
/// Matched by digest and never by name: two packs can build programs of one name, and only the
/// digest says which of them this run executed.
fn source_of(model: &RunModel) -> Option<Source> {
    let digests: Vec<&str> = model.binaries.iter().map(|record| bare(&record.blake3)).collect();
    let mut names: Vec<String> = std::fs::read_dir("targets")
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("target.toml").is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names.into_iter().find_map(|name| {
        let pack = crate::targets::resolve(&name).ok()?;
        digests.contains(&pack.digest.as_str()).then_some(Source {
            pack: pack.name,
            repository: pack.repository,
            commit: pack.commit,
        })
    })
}

/// A digest as a pack writes it: a bundle puts the algorithm in front, a pack writes the hex alone.
fn bare(digest: &str) -> &str {
    digest.strip_prefix("blake3:").unwrap_or(digest)
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
    println!("{out}");
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
        // One bundle named is one the caller expects read: a typo is an error, never an empty log
        // that says there is nothing to fix.
        RunModel::of(&read(given)?).map_err(|e| CliError::Usage(e.to_string()))?;
        vec![given.to_owned()]
    };
    paths.sort();

    let mut groups: std::collections::BTreeMap<String, Vec<(u64, PathBuf, RunModel)>> =
        std::collections::BTreeMap::new();
    for path in &paths {
        // A bundle that does not read is named and left out, as `bohr findings` leaves it out.
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
    println!("{out} · {} result{}", findings.len(), if findings.len() == 1 { "" } else { "s" });
    Ok(crate::exit::CLEAN)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two spellings of one digest name one build.
    #[test]
    fn a_bundle_digest_matches_the_pack_that_wrote_it_bare() {
        assert_eq!(bare("blake3:b1f4"), "b1f4");
        assert_eq!(bare("b1f4"), "b1f4");
    }
}
