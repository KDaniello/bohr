//! Target packs: what `bohr hunt <name>` builds, checks and runs.
//!
//! A pack is `targets/<name>/target.toml`: where the program comes from, how it is built, the
//! blake3 digest the build must produce, and the scenario a campaign runs. A binary with another
//! digest is refused before it runs.
//!
//! A pack fetches nothing beyond what its build command does. A build that does not reproduce the
//! digest is reported with both digests and not run.

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
    /// The checks `bohr check` holds the pack to, read by [`crate::subjects`].
    check: Option<toml::Table>,
    scope: Option<RawScope>,
}

/// Which of the program's modes are not run in production.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScope {
    /// Words of a command line that put the program in a mode nobody runs in production.
    #[serde(default)]
    nonproduction: Vec<String>,
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
    /// The pack's `[check]` table, as written.
    pub(crate) checks: Option<toml::Table>,
    /// Command-line words that put the program in a mode nobody runs in production; a finding of
    /// a run with one of them is out of scope.
    pub(crate) nonproduction: Vec<String>,
}

/// Where a pack named bare is read from: `targets/` here, or else the packs a release installed.
///
/// `targets/` here first, then `$BOHR_HOME/packs` or `~/.bohr/packs`, where `bohrup` installs them.
/// A name found in neither is reported at the first location.
fn installed(name: &str) -> PathBuf {
    let here = Path::new("targets").join(name).join("target.toml");
    if here.is_file() {
        return here;
    }
    installed_packs()
        .map(|packs| packs.join(name).join("target.toml"))
        .filter(|file| file.is_file())
        .unwrap_or(here)
}

/// Where a release's packs are installed: `$BOHR_HOME/packs/targets`, or `~/.bohr/packs/targets`.
fn installed_packs() -> Option<PathBuf> {
    std::env::var_os("BOHR_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".bohr")))
        .map(|home| home.join("packs/targets"))
}

/// Finds a pack by name under `targets/`, or by the path of its directory or its file.
pub(crate) fn resolve(name: &str) -> Result<Pack, CliError> {
    // A bare name is a pack, even if a file of that name exists here.
    let given = Path::new(name);
    // Checked by components, since a Windows path has no slash.
    let bare = given.components().count() == 1 && !name.ends_with(".toml");
    let file = if bare {
        installed(name)
    } else if given.is_dir() {
        given.join("target.toml")
    } else {
        given.to_owned()
    };
    let text = std::fs::read_to_string(&file).map_err(|e| {
        CliError::refused(format!(
            "no target pack at {}: {e}; packs live in targets/<name>/",
            file.display()
        ))
    })?;
    let raw: RawPack = toml::from_str(&text)
        .map_err(|e| CliError::refused(format!("{} does not parse: {e}", file.display())))?;
    // The repository is two directories above `target.toml`, on its absolute path.
    let file = std::path::absolute(&file).unwrap_or(file);
    let dir = file.parent().unwrap_or(Path::new("."));
    let root = dir.parent().and_then(Path::parent).unwrap_or(Path::new(".")).to_owned();
    if !ORACLES.contains(&raw.judge.oracle.as_str()) {
        return Err(CliError::refused(format!(
            "{}: judge.oracle {:?} is none of {}",
            file.display(),
            raw.judge.oracle,
            ORACLES.join(", ")
        )));
    }
    if raw.bench.as_ref().is_some_and(|b| b.argv.is_empty()) {
        return Err(CliError::refused(format!("{}: bench.argv is empty", file.display())));
    }
    if raw.build.command.is_empty() {
        return Err(CliError::refused(format!("{}: build.command is empty", file.display())));
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
        checks: raw.check,
        nonproduction: raw.scope.unwrap_or_default().nonproduction,
        root,
    })
}

/// The blake3 digest of a file, in hex.
pub(crate) fn digest_of(path: &Path) -> Result<String, CliError> {
    let bytes = std::fs::read(path)
        .map_err(|e| CliError::refused(format!("cannot read {}: {e}", path.display())))?;
    Ok(blake3_hex(&bytes))
}

fn blake3_hex(bytes: &[u8]) -> String {
    bohr_types::Digest::of(bytes).to_hex()
}

/// The binary a campaign runs: the one given, or the pack's own, built where it is missing.
///
/// A binary that differs from the pack is refused with both digests, never rebuilt over.
pub(crate) fn binary_for(pack: &Pack, given: Option<&Path>) -> Result<PathBuf, CliError> {
    let path = given.map_or_else(|| pack.binary.clone(), Path::to_owned);
    if !path.is_file() {
        if given.is_some() {
            return Err(CliError::refused(format!("no binary at {}", path.display())));
        }
        build(pack)?;
        if !path.is_file() {
            return Err(CliError::refused(format!(
                "{}'s build ran and left no binary at {}",
                pack.name,
                path.display()
            )));
        }
    }
    let found = digest_of(&path)?;
    if found != pack.digest {
        return Err(CliError::refused(format!(
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
    let mut command = std::process::Command::new(&pack.build[0]);
    command.args(&pack.build[1..]).current_dir(&pack.root);
    // Under `--json`, the build's output goes to stderr.
    if crate::json::on() {
        command.stdout(std::io::stderr());
    }
    let status = command
        .status()
        .map_err(|e| CliError::refused(format!("cannot run {}: {e}", pack.build[0])))?;
    if !status.success() {
        return Err(CliError::refused(format!("{}'s build failed: {status}", pack.name)));
    }
    Ok(())
}

/// The pack, here or installed, whose binary has one of `digests`, where one has.
///
/// Packs are tried in name order, so two packs of one binary resolve the same way every time.
pub(crate) fn pack_of(digests: &[&str]) -> Option<Pack> {
    let digests: Vec<String> = digests.iter().map(|d| bare(d).to_owned()).collect();
    // The packs here and the packs a release installed, as `resolve` finds them.
    let mut names: Vec<String> = [Some(PathBuf::from("targets")), installed_packs()]
        .into_iter()
        .flatten()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("target.toml").is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names.dedup();
    names.into_iter().find_map(|name| {
        let pack = resolve(&name).ok()?;
        digests.contains(&pack.digest).then_some(pack)
    })
}

/// A digest without its `blake3:` prefix, as a pack writes it.
fn bare(digest: &str) -> &str {
    digest.strip_prefix("blake3:").unwrap_or(digest)
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

    /// A pack's paths are its repository's, and an unknown key is refused.
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
