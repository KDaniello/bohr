//! SARIF 2.1.0: findings as code scanning reads them.
//!
//! One result per finding group, never one per bundle. A campaign reaches one defect from many
//! seeds, and a code-scanning view that listed each seed would bury the defect under its own
//! repetitions — the same reason `bohr findings` groups. Each result is led by its group's
//! smallest bundle, which is the one to replay.
//!
//! The rule is the violation's code. The location is the guest's own source line where its records
//! name one; where they do not, the bundle itself, because a result with no location is one that
//! code scanning refuses rather than shows.
//!
//! What this does not do: map a crash to a source line. A signal or an exit status carries no
//! line, and symbolising the guest's stack is not something a stored run can do.

use serde_json::{Value, json};

use crate::Location;

/// One finding group, as a result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// The violation's code — `BOHR-2012`, `GuestCrashed` — which becomes the rule.
    pub code: String,
    /// What kind of thing went wrong, in words, for every group under the rule.
    pub class: String,
    /// What went wrong in this group, in words.
    pub headline: String,
    /// The version of bohr that recorded the bundle.
    pub recorded_by: String,
    /// What tells this group from others of its code.
    pub signature: String,
    /// Where the promise is written, where the guest's records say.
    pub location: Option<Location>,
    /// The group's smallest bundle, as the path a replay is given.
    pub bundle: String,
    /// What that bundle replays to.
    pub digest: String,
    /// How many events it records.
    pub events: u64,
    /// How many bundles the group holds.
    pub members: usize,
}

/// The SARIF log for `findings`, as `bohr` version `version` reports them.
#[must_use]
pub fn sarif(version: &str, findings: &[Finding]) -> Value {
    let mut rules: Vec<&Finding> = Vec::new();
    for finding in findings {
        if !rules.iter().any(|rule| rule.code == finding.code) {
            rules.push(finding);
        }
    }
    let rule_values: Vec<Value> = rules
        .iter()
        .map(|rule| {
            json!({
                "id": rule.code,
                "shortDescription": { "text": rule.class },
                "defaultConfiguration": { "level": "error" },
            })
        })
        .collect();

    let results: Vec<Value> = findings
        .iter()
        .map(|finding| {
            let index = rules.iter().position(|rule| rule.code == finding.code).unwrap_or(0);
            let location = match &finding.location {
                Some(at) => json!({
                    "physicalLocation": {
                        "artifactLocation": { "uri": uri(&at.file) },
                        "region": { "startLine": at.line.max(1) },
                    },
                    "logicalLocations": [{ "name": at.function, "kind": "function" }],
                }),
                None => json!({
                    "physicalLocation": { "artifactLocation": { "uri": uri(&finding.bundle) } },
                }),
            };
            let seeds = if finding.members == 1 {
                "one run".to_owned()
            } else {
                format!("{} runs", finding.members)
            };
            json!({
                "ruleId": finding.code,
                "ruleIndex": index,
                "level": "error",
                "message": {
                    "text": format!(
                        "{} — found in {seeds}. Reproduce with `bohr replay {}`, which checks the \
                         run against digest {}.",
                        finding.headline, finding.bundle, finding.digest
                    ),
                },
                "locations": [location],
                // Stable across runs of one defect, so code scanning keeps one alert open rather
                // than opening one per campaign.
                "partialFingerprints": { "bohrSignature/v1": fingerprint(&finding.signature) },
                "properties": {
                    "bundle": finding.bundle,
                    "traceDigest": finding.digest,
                    "events": finding.events,
                    "bundles": finding.members,
                    "recordedBy": finding.recorded_by,
                },
            })
        })
        .collect();

    json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "bohr",
                    "version": version,
                    "informationUri": "https://github.com/KDaniello/bohr",
                    "rules": rule_values,
                },
            },
            "results": results,
        }],
    })
}

/// A path as a URI reference: separators made forward, and every byte outside the unreserved set
/// and `/` percent-encoded. An absolute path becomes a `file:` URI, a relative one stays relative,
/// which is what code scanning resolves against the repository.
fn uri(path: &str) -> String {
    let path = path.replace('\\', "/");
    let mut out = String::new();
    let drive = path.as_bytes().get(1) == Some(&b':') && path.as_bytes()[0].is_ascii_alphabetic();
    if drive {
        out.push_str("file:///");
    } else if path.starts_with('/') {
        out.push_str("file://");
    }
    for byte in path.bytes() {
        let keep =
            byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) || (drive && byte == b':');
        if keep {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A signature as sixteen hex digits: FNV-1a, which is fixed by definition and needs no crate.
fn fingerprint(signature: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in signature.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(code: &str, signature: &str, location: Option<Location>) -> Finding {
        Finding {
            code: code.to_owned(),
            class: "crashed".to_owned(),
            headline: "a guest failed: exit status 1".to_owned(),
            recorded_by: "0.7.0".to_owned(),
            signature: signature.to_owned(),
            location,
            bundle: "findings/a.bohr".to_owned(),
            digest: "00ff".to_owned(),
            events: 18,
            members: 3,
        }
    }

    /// Two groups of one code are two results under one rule, and every result has a location.
    #[test]
    fn one_rule_per_code_and_one_result_per_group() {
        let at = Location { file: "src/log.rs".to_owned(), function: "append".to_owned(), line: 9 };
        let log = sarif(
            "0.7.0",
            &[
                finding("GuestCrashed", "n0 status 1", None),
                finding("GuestCrashed", "n1 status 1", None),
                finding("BOHR-2012", "log is monotonic", Some(at)),
            ],
        );
        let run = &log["runs"][0];
        assert_eq!(log["version"], "2.1.0");
        assert_eq!(run["tool"]["driver"]["rules"].as_array().map(Vec::len), Some(2));
        let results = run["results"].as_array().expect("results");
        assert_eq!(results.len(), 3);
        assert_eq!(results[2]["ruleIndex"], 1);
        for result in results {
            assert!(
                result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"].is_string()
            );
        }
        assert_eq!(
            results[2]["locations"][0]["physicalLocation"]["region"]["startLine"], 9,
            "a promise's own line was not the location"
        );
        assert_ne!(
            results[0]["partialFingerprints"]["bohrSignature/v1"],
            results[1]["partialFingerprints"]["bohrSignature/v1"]
        );
    }

    /// A location code scanning can resolve: relative stays relative, absolute is a `file:` URI,
    /// and nothing outside the unreserved set is left raw.
    #[test]
    fn a_path_becomes_a_uri_reference() {
        assert_eq!(uri("src/log.rs"), "src/log.rs");
        assert_eq!(uri("src\\my log.rs"), "src/my%20log.rs");
        assert_eq!(uri("/home/a/100%.rs"), "file:///home/a/100%25.rs");
        assert_eq!(uri("C:\\w\\a.rs"), "file:///C:/w/a.rs");
    }

    /// FNV-1a's published value for the empty string, so the fingerprint cannot drift.
    #[test]
    fn the_fingerprint_is_fnv1a() {
        assert_eq!(fingerprint(""), "cbf29ce484222325");
        assert_eq!(fingerprint("a"), "af63dc4c8601ec8c");
    }
}
