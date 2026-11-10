//! `bohr confirm`: writes a script that stages a stored finding on a plain Linux host.
//!
//! The script starts the recorded programs as ordinary processes and delivers the recorded node
//! faults with signals: `kill -9` for a kill, `SIGSTOP` and `SIGCONT` for a freeze. It restarts a
//! node where the run restarted it, repeats the whole run, and counts how often the recorded exit
//! status comes back.
//!
//! A killed process keeps its unsynced writes in the page cache; a crash on the model loses them.
//! For a run with a kill, the script gives each node a dm-flakey device of its own, drops that
//! node's writes at its kill and remounts, so only synced data survives, on that node alone.
//!
//! Virtual time differs from real time. Each repetition draws a fault's moment between zero and
//! twice the recorded one, and the rate covers that window.
//!
//! The script does not stage partitions, lost or delayed packets, disk errors and tears, clock
//! faults or probe verdicts. It lists the ones the run had and judges exit statuses only.

use bohr_findings::confirm::{Node, Plan, script};
use bohr_report::RunModel;

use crate::CliError;

/// Refuses a node program that is not, byte for byte, one the run recorded.
///
/// A bundle cannot have the script start another program than the one that ran. It does not make
/// a stranger's bundle safe: the program's arguments are the bundle's, and the script runs them as
/// root, so the script is read before it is run.
fn check_programs(nodes: &[Node], binaries: &[bohr_trace::BinaryRecord]) -> Result<(), String> {
    for (argv, _) in nodes {
        let Some(program) = argv.first() else { continue };
        let recorded = binaries
            .iter()
            .find(|b| b.path == *program && b.role == bohr_trace::BinaryRole::Executable)
            .ok_or_else(|| format!("{program} is not a program the run recorded"))?;
        let bytes = std::fs::read(program).map_err(|e| {
            format!("cannot read {program} to check it is the one that ran, from here: {e}")
        })?;
        if bohr_types::Digest::of(&bytes).to_string() != recorded.blake3 {
            return Err(format!("{program} is not the build the run recorded"));
        }
    }
    Ok(())
}

/// Writes the script for the bundle at `path` to `out`.
pub(crate) fn confirm(path: &str, out: &str, runs: u32) -> Result<u8, CliError> {
    let bytes = std::fs::read(path)
        .map_err(|e| CliError::usage(format!("BOHR-1002: cannot read {path}: {e}")))?;
    let bundle = bohr_trace::Bundle::parse(&bytes).map_err(|e| CliError::usage(e.to_string()))?;
    let model = RunModel::of(&bundle).map_err(|e| CliError::usage(e.to_string()))?;
    let refused = |why: String| CliError::usage(format!("BOHR-5003: {path}: {why}"));
    let plan = Plan::of(&model).map_err(refused)?;
    check_programs(&plan.nodes, &model.binaries).map_err(refused)?;
    let text = script(&plan, path, runs);
    std::fs::write(out, &text)
        .map_err(|e| CliError::usage(format!("BOHR-1002: cannot write {out}: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o755));
    }
    if crate::json::on() {
        crate::json::emit(
            "confirm",
            serde_json::json!({
                "bundle": path,
                "written": out,
                "nodes": plan.nodes.len(),
                "faults": plan.actions.len(),
                "power_loss": plan.crashes(),
                "expect": plan.expect.iter().map(|(n, s)| serde_json::json!({ "node": n, "status": s })).collect::<Vec<_>>(),
                "not_staged": plan.unstaged,
                "runs": runs,
            }),
        );
    } else {
        println!("{out}");
        eprintln!(
            "  it runs the run's own commands as root; read it first if the bundle is not yours"
        );
        if plan.crashes() {
            eprintln!(
                "  the run crashed a node: run the script as root, which it needs for dm-flakey"
            );
        }
        if !plan.unstaged.is_empty() {
            eprintln!("  not staged, though the run had them: {}", plan.unstaged.join(", "));
        }
    }
    Ok(crate::exit::CLEAN)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A program that is not, byte for byte, the one the run recorded is refused.
    #[test]
    fn only_the_recorded_program_is_staged() {
        let dir = std::env::temp_dir().join(format!("bohr-confirm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let program = dir.join("server").to_string_lossy().into_owned();
        std::fs::write(&program, b"the build that ran").expect("written");
        let record = |blake3: String| bohr_trace::BinaryRecord {
            path: program.clone(),
            blake3,
            role: bohr_trace::BinaryRole::Executable,
            args: Vec::new(),
        };
        let ran = bohr_types::Digest::of(b"the build that ran").to_string();
        let nodes = vec![(vec![program.clone()], Vec::new())];
        assert!(check_programs(&nodes, &[record(ran)]).is_ok());
        let other = bohr_types::Digest::of(b"another build").to_string();
        assert!(check_programs(&nodes, &[record(other)]).is_err(), "another build was staged");
        let named = vec![(vec!["/usr/bin/python3".to_owned()], Vec::new())];
        assert!(check_programs(&named, &[]).is_err(), "an unrecorded program was staged");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
