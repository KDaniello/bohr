//! Target packs: what `bohr hunt <name>` builds, checks and runs.
//!
//! A pack is `targets/<name>/target.toml` in this repository: where the program comes from, how it
//! is built, which binary that build must produce, and the scenario a campaign runs it in. The
//! binary is pinned by its blake3 digest, the hash every artefact here is pinned by, so a pack
//! names exactly one program and a binary that differs from it is refused before it runs.
//!
//! What a pack does not do: fetch anything itself beyond what its build command does, or accept a
//! binary nobody can check. A build that does not reproduce the pinned digest on another machine
//! is reported with both digests, not run.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::CliError;

/// A pack as its file states it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPack {
    source: RawSource,
    build: RawBuild,
    run: RawRun,
    judge: RawJudge,
    bench: Option<RawBench>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSource {
    repository: String,
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBuild {
    command: Vec<String>,
    binary: String,
    blake3: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRun {
    scenario: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawJudge {
    oracle: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBench {
    argv: Vec<String>,
}

/// The oracles a pack may name, each a section its scenario must carry or a judgement every run
/// already makes.
const ORACLES: [&str; 5] = ["crash", "durability", "liveness", "property", "chain"];

/// A pack, resolved against the repository it lives in.
#[derive(Clone, Debug)]
pub(crate) struct Pack {
    /// What it is called: its directory's name.
    pub(crate) name: String,
    /// The directory every path in the pack is relative to.
    pub(crate) root: PathBuf,
    /// Where the program's source is, and the commit built.
    pub(crate) repository: String,
    pub(crate) commit: String,
    /// The command that builds the binary, run in `root`.
    pub(crate) build: Vec<String>,
    /// Where the build leaves the binary.
    pub(crate) binary: PathBuf,
    /// The binary's blake3 digest, in hex.
    pub(crate) digest: String,
    /// The scenario a campaign runs.
    pub(crate) scenario: PathBuf,
    /// What judges a run.
    pub(crate) oracle: String,
}

fn refused(text: String) -> CliError {
    CliError::Usage(format!("BOHR-1002: {text}"))
}

/// Finds a pack by name under `targets/`, or by the path of its directory or its file.
pub(crate) fn resolve(name: &str) -> Result<Pack, CliError> {
    // A bare name is a pack under `targets/`, even where a file of that name sits here: `reth` is
    // the pack, and a `./reth` binary beside it is not a pack file.
    let given = Path::new(name);
    // A path by its components rather than by a slash, which a Windows path does not contain.
    let bare = given.components().count() == 1 && !name.ends_with(".toml");
    let file = if bare {
        Path::new("targets").join(name).join("target.toml")
    } else if given.is_dir() {
        given.join("target.toml")
    } else {
        given.to_owned()
    };
    let text = std::fs::read_to_string(&file).map_err(|e| {
        refused(format!("no target pack at {}: {e}; packs live in targets/<name>/", file.display()))
    })?;
    let raw: RawPack = toml::from_str(&text)
        .map_err(|e| refused(format!("{} does not parse: {e}", file.display())))?;
    // `targets/<name>/target.toml`: the repository is two directories up from the file, counted on
    // its absolute path so a pack given as `./target.toml` from its own directory resolves too.
    let file = std::path::absolute(&file).unwrap_or(file);
    let dir = file.parent().unwrap_or(Path::new("."));
    let root = dir.parent().and_then(Path::parent).unwrap_or(Path::new(".")).to_owned();
    if !ORACLES.contains(&raw.judge.oracle.as_str()) {
        return Err(refused(format!(
            "{}: judge.oracle {:?} is none of {}",
            file.display(),
            raw.judge.oracle,
            ORACLES.join(", ")
        )));
    }
    if raw.bench.as_ref().is_some_and(|b| b.argv.is_empty()) {
        return Err(refused(format!("{}: bench.argv is empty", file.display())));
    }
    if raw.build.command.is_empty() {
        return Err(refused(format!("{}: build.command is empty", file.display())));
    }
    Ok(Pack {
        name: dir.file_name().map_or_else(|| name.to_owned(), |n| n.to_string_lossy().into_owned()),
        repository: raw.source.repository,
        commit: raw.source.commit,
        build: raw.build.command,
        binary: root.join(raw.build.binary),
        digest: raw.build.blake3.to_lowercase(),
        scenario: root.join(raw.run.scenario),
        oracle: raw.judge.oracle,
        root,
    })
}

/// The blake3 digest of a file, in hex.
pub(crate) fn digest_of(path: &Path) -> Result<String, CliError> {
    let bytes =
        std::fs::read(path).map_err(|e| refused(format!("cannot read {}: {e}", path.display())))?;
    Ok(blake3_hex(&bytes))
}

fn blake3_hex(bytes: &[u8]) -> String {
    bohr_types::Digest::of(bytes).to_hex()
}

/// The binary a campaign runs: the one given, or the pack's own, built where it is missing.
///
/// **A binary that differs from the pack is refused, never rebuilt over.** Rebuilding would hide
/// that the file someone handed in is not the program the pack names; refusing says so with both
/// digests, which is what a person needs to decide which one is wrong.
pub(crate) fn binary_for(pack: &Pack, given: Option<&Path>) -> Result<PathBuf, CliError> {
    let path = given.map_or_else(|| pack.binary.clone(), Path::to_owned);
    if !path.is_file() {
        if given.is_some() {
            return Err(refused(format!("no binary at {}", path.display())));
        }
        build(pack)?;
        if !path.is_file() {
            return Err(refused(format!(
                "{}'s build ran and left no binary at {}",
                pack.name,
                path.display()
            )));
        }
    }
    let found = digest_of(&path)?;
    if found != pack.digest {
        return Err(refused(format!(
            "{} has blake3 {found}, and pack {} names {}; it is not the program the pack names",
            path.display(),
            pack.name,
            pack.digest
        )));
    }
    Ok(path)
}

/// Runs the pack's build command in its repository.
fn build(pack: &Pack) -> Result<(), CliError> {
    eprintln!("building {} from {} at {}", pack.name, pack.repository, pack.commit);
    let status = std::process::Command::new(&pack.build[0])
        .args(&pack.build[1..])
        .current_dir(&pack.root)
        .status()
        .map_err(|e| refused(format!("cannot run {}: {e}", pack.build[0])))?;
    if !status.success() {
        return Err(refused(format!("{}'s build failed: {status}", pack.name)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack_in(dir: &Path, text: &str) -> Result<Pack, CliError> {
        let at = dir.join("targets").join("demo");
        std::fs::create_dir_all(&at).unwrap();
        std::fs::write(at.join("target.toml"), text).unwrap();
        resolve(&at.join("target.toml").to_string_lossy())
    }

    const PACK: &str = r#"
[source]
repository = "https://example.org/demo"
commit = "0123abc"
[build]
command = ["true"]
binary = "bin/demo"
blake3 = "AB"
[run]
scenario = "demo.toml"
[judge]
oracle = "crash"
"#;

    /// A pack's paths are its repository's, and an unknown key is refused rather than skipped.
    #[test]
    fn a_pack_resolves_against_its_repository() {
        let dir = std::env::temp_dir().join(format!("bohr-pack-{}", std::process::id()));
        let pack = pack_in(&dir, PACK).unwrap();
        assert_eq!(pack.name, "demo");
        assert_eq!(pack.binary, dir.join("bin/demo"));
        assert_eq!(pack.scenario, dir.join("demo.toml"));
        assert_eq!(pack.digest, "ab");
        assert!(pack_in(&dir, &PACK.replace("[judge]", "[judge]\nweight = 1")).is_err());
        assert!(pack_in(&dir, &PACK.replace("\"crash\"", "\"vibes\"")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A binary whose digest differs is refused with both digests named.
    #[test]
    fn a_changed_binary_is_refused() {
        let dir = std::env::temp_dir().join(format!("bohr-pack-bin-{}", std::process::id()));
        let pack = pack_in(&dir, PACK).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/demo"), b"not the program").unwrap();
        let err = binary_for(&pack, None).unwrap_err().to_string();
        assert!(err.contains(&blake3_hex(b"not the program")), "{err}");
        assert!(err.contains("names ab"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
