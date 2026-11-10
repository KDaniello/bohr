//! `bohr init -- ./node`: a first scenario, written from what a survey of the program finds.
//!
//! A starting point. It writes the cluster shape, a horizon, the faults the program's calls can
//! meet (disk faults if it syncs, network faults if it talks, crashes if it keeps state) and one
//! comment per wall the survey found. A dynamic program gets its loader's libraries copied into a
//! prepared state, pinned by digest.
//!
//! The probabilities are first guesses and are marked as such.

use std::path::{Path, PathBuf};

use crate::{CliError, execute, survey};

fn refused(text: String) -> CliError {
    CliError::Usage(format!("BOHR-1002: {text}"))
}

/// The ELF interpreter a binary names, where it names one: a dynamic binary's loader.
pub(crate) fn interpreter(bytes: &[u8]) -> Option<String> {
    let word = |at: usize, n: usize| -> Option<u64> {
        let raw = bytes.get(at..at.checked_add(n)?)?;
        let mut out = [0u8; 8];
        out[..n].copy_from_slice(raw);
        Some(u64::from_le_bytes(out))
    };
    // ELF64, little-endian: the only kind this build runs.
    if bytes.get(..4) != Some(b"\x7fELF".as_slice()) || bytes.get(4) != Some(&2) {
        return None;
    }
    let (phoff, phentsize, phnum) =
        (word(0x20, 8)? as usize, word(0x36, 2)? as usize, word(0x38, 2)? as usize);
    // Bounds-checked throughout; a header pointing past the file gives `None`.
    (0..phnum).find_map(|n| {
        let header = phoff.checked_add(n.checked_mul(phentsize)?)?;
        // `PT_INTERP`.
        (word(header, 4)? == 3).then_some(())?;
        let offset = usize::try_from(word(header.checked_add(8)?, 8)?).ok()?;
        let size = usize::try_from(word(header.checked_add(32)?, 8)?).ok()?;
        let path = bytes.get(offset..offset.checked_add(size)?)?;
        Some(String::from_utf8_lossy(path).trim_end_matches('\0').to_owned())
    })
}

/// Copies what a dynamic binary's loader resolves into `state`, laid out where it looks.
fn libraries_into(binary: &str, state: &Path) -> Result<usize, CliError> {
    let listed = std::process::Command::new("ldd")
        .arg(binary)
        .output()
        .map_err(|e| refused(format!("cannot ask ldd about {binary}: {e}")))?;
    let mut copied = 0;
    for line in String::from_utf8_lossy(&listed.stdout).lines() {
        // A library the loader cannot find is reported now; the scenario could not start.
        if line.contains("not found") {
            return Err(refused(format!(
                "{binary} needs a library this host does not have: {}",
                line.trim()
            )));
        }
        let path = match line.split_once("=>") {
            Some((_, rest)) => rest.split_whitespace().next(),
            None => line.split_whitespace().next(),
        };
        let Some(path) = path.filter(|p| p.starts_with('/')) else { continue };
        let source = Path::new(path);
        if !source.is_file() {
            continue;
        }
        let into = state.join(path.trim_start_matches('/'));
        if let Some(parent) = into.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| refused(format!("cannot use {}: {e}", parent.display())))?;
        }
        std::fs::copy(source, &into).map_err(|e| refused(format!("cannot copy {path}: {e}")))?;
        copied += 1;
    }
    Ok(copied)
}

/// The syscall numbers a run made, from its log.
fn calls_made(log: &[u8]) -> std::collections::BTreeSet<u32> {
    let (events, _) = bohr_trace::read_events_lossy(log);
    events
        .iter()
        .filter_map(|event| match event.kind {
            bohr_trace::EventKind::Syscall { nr, .. } => Some(nr),
            _ => None,
        })
        .collect()
}

/// Writes a first scenario for `program` to `out`, and says what it found.
pub(crate) fn init(program: Vec<String>, nodes: u32, out: &str) -> Result<u8, CliError> {
    let Some(binary) = program.first().cloned() else {
        return Err(CliError::Usage("BOHR-1002: init needs a program, after `--`".to_owned()));
    };
    let out = PathBuf::from(out);
    if out.exists() {
        return Err(refused(format!(
            "{} exists, and init does not write over a scenario",
            out.display()
        )));
    }
    let bytes =
        std::fs::read(&binary).map_err(|e| refused(format!("cannot read {binary}: {e}")))?;
    let nodes = nodes.max(1);

    let mut head = format!(
        "# A first scenario for {binary}, written by `bohr init` from one survey of it.\n\
         #\n\
         # A starting point: the shape, a horizon, and the faults its calls can meet, at odds that\n\
         # are a first guess. Tune them, and read the walls at the end before trusting a clean run.\n\n\
         [run]\nhorizon_ticks = 60_000_000_000    # a minute of virtual time\n\n"
    );
    let argv: Vec<String> = program.iter().map(|a| format!("{a:?}")).collect();
    head.push_str(&format!("[cluster]\nnodes = {nodes}\nargv = [{}]\n", argv.join(", ")));

    // A dynamic binary runs with the libraries it was written against, pinned.
    let mut state_text = String::new();
    let mut prepared = serde_json::Value::Null;
    if let Some(loader) = interpreter(&bytes) {
        let stem =
            out.file_stem().map_or_else(|| "scenario".into(), |s| s.to_string_lossy().into_owned());
        let state = out.with_file_name(format!("{stem}-state"));
        if state.exists() {
            return Err(refused(format!(
                "{} exists, and init does not write over a prepared state",
                state.display()
            )));
        }
        let copied = libraries_into(&binary, &state)?;
        let image = bohr_world::StateImage::from_dir(&state, u64::MAX)
            .map_err(|e| refused(e.to_string()))?;
        state_text = format!(
            "\n# {binary} is dynamically linked against {loader}: {copied} libraries its loader asks\n\
             # for, copied from this host and pinned, so the run loads exactly these.\n\
             [state]\ndir = {:?}\ndigest = \"{}\"\ncwd = \"/\"\n",
            state.to_string_lossy(),
            image.digest().to_hex()
        );
        prepared = serde_json::json!({
            "loader": loader,
            "dir": state.display().to_string(),
            "digest": image.digest().to_hex(),
            "libraries": copied,
        });
    }

    // One survey in the world written so far, with the log kept, to learn what it calls.
    let first = format!("{head}{state_text}");
    let loaded = bohr_world::config::load(&first)?;
    let completed = survey::surface(
        program.clone(),
        1,
        Some(&loaded),
        &first,
        Some(2_000_000),
        execute::Record::Log,
    )?;
    let calls = calls_made(&completed.log);
    let any = |set: &[u32]| set.iter().any(|nr| calls.contains(nr));

    let mut faults = String::new();
    // fsync, fdatasync, msync, sync_file_range: it trusts the disk to keep something.
    let syncs = any(&[74, 75, 26, 277]);
    // socket, connect, sendto, recvfrom, sendmsg, recvmsg: it talks to someone.
    let talks = any(&[41, 42, 44, 45, 46, 47]);
    if syncs {
        faults.push_str(
            "\n# It syncs, so the disk can fail it: a sync that reports EIO, and one that stalls.\n\
             [disk]\nsync_semantics = \"strict\"\neio_probability = \"1/1000\"\nstall_probability = \"1/100\"\nstall_mean_ticks = 5_000_000\n",
        );
    }
    if talks && nodes > 1 {
        faults.push_str(
            "\n# It talks across nodes, so the network can lose, repeat and delay what it says.\n\
             [network]\nlatency_mean_ticks = 100_000\nlatency_jitter_ticks = 50_000\ndrop_probability = \"1/1000\"\nduplicate_probability = \"1/10000\"\n",
        );
    }
    if syncs || nodes > 1 {
        faults.push_str(
            "\n# A node can die where it stands and come back, which is what tests what it kept.\n\
             [faults]\nkill_probability = \"1/1000\"\nrestart_after_ticks = 50_000_000\nbudget_per_run = 2\n",
        );
    }

    let mut walls = String::from(
        "\n# The walls one survey found: calls this build cannot answer for this program.\n",
    );
    if completed.surveyed.is_empty() {
        walls.push_str("#   none: every call it made was answered.\n");
    }
    for (nr, count) in &completed.surveyed {
        let (name, wall) = survey::describe(*nr);
        walls.push_str(&format!("#   {name} ({nr}), {count} calls: {wall}\n"));
    }
    if completed.report.outcome != bohr_trace::Outcome::Clean {
        walls.push_str(&format!(
            "#   and the survey did not run to completion: {:?}\n",
            completed.report.outcome
        ));
    }

    let text = format!("{first}{faults}{walls}");
    bohr_world::config::load(&text)?;
    std::fs::write(&out, &text)
        .map_err(|e| refused(format!("cannot write {}: {e}", out.display())))?;
    eprintln!(
        "wrote {} · {} node(s) · {} walls · disk faults {} · network faults {}",
        out.display(),
        nodes,
        completed.surveyed.len(),
        if syncs { "on" } else { "off" },
        if talks && nodes > 1 { "on" } else { "off" }
    );
    if crate::json::on() {
        let verdict = bohr_report::narrate::verdict(&completed.report.outcome);
        crate::json::emit(
            "init",
            serde_json::json!({
                "path": out.display().to_string(),
                "program": program,
                "nodes": nodes,
                "state": prepared,
                "faults": {
                    "disk": syncs,
                    "network": talks && nodes > 1,
                    "crashes": syncs || nodes > 1,
                },
                "survey": {
                    "events": completed.counters.events,
                    "completed": completed.report.outcome == bohr_trace::Outcome::Clean,
                    "outcome": verdict.code,
                    "walls": survey::walls_json(&completed.surveyed),
                },
            }),
        );
    }
    Ok(crate::exit::CLEAN)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A static binary names no loader; this test binary, built by cargo, names its own or none.
    #[test]
    fn a_binary_says_whether_it_has_a_loader() {
        assert_eq!(interpreter(b"not an elf"), None);
        let me = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        if let Some(loader) = interpreter(&me) {
            assert!(loader.starts_with('/'), "{loader}");
        }
    }
}
