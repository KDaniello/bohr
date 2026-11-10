//! A disclosure draft: one finding as the Markdown a maintainer receives.
//!
//! Not a report of the run but a letter about it. The page answers "what happened in this run";
//! a maintainer reading an issue asks something else first — is this my program, can I make it
//! happen, and how far can I believe it — so the draft is ordered by those three questions, and
//! the evidence of the run comes after them.
//!
//! What this does not do: send anything, judge severity, or say why the program failed. The
//! steps are what came before the violation, gathered by rule, and the draft says so where a
//! reader would otherwise take them for a diagnosis.

use std::fmt::Write as _;

use bohr_trace::EventKind;

use crate::{
    RunModel, describe,
    narrate::{self, Story},
};

/// Where the program under test came from, where something on the host says so.
///
/// The bundle names its programs by path and digest and nothing else: a commit is a fact about
/// how a binary was built, and only the target pack that built it knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// The pack's name.
    pub pack: String,
    /// Where the source lives.
    pub repository: String,
    /// What it was built from.
    pub commit: String,
}

/// The draft for one stored run.
///
/// `bundle` is the name the reproduction command uses; `source`, where a target pack names the
/// build by its digest.
#[must_use]
pub fn disclosure(model: &RunModel, bundle: &str, source: Option<&Source>) -> String {
    let story = narrate::story(model, bundle);
    let manifest = &model.manifest;
    let mut out = String::new();

    let programs = programs(model);
    let named = programs.first().map_or("the program", |(name, _)| name.as_str());
    let _ = writeln!(out, "# {}: {}\n", named, story.verdict.headline);
    out.push_str(
        "> Draft written by `bohr report --disclosure` from one recorded run. It is meant to be \
         read, edited and sent by a person; nothing in it has been sent anywhere.\n\n",
    );

    target(&mut out, &programs, source);
    found(&mut out, model, &story);
    reproduce(&mut out, model, bundle);
    run(&mut out, model, &story);
    limits(&mut out, model);

    let _ = writeln!(
        out,
        "---\n\nbohr {} · tape ABI {} · bundle format {} · run `{}`",
        manifest.tool_version, manifest.tape_abi, manifest.bundle_format, manifest.run_id
    );
    out
}

/// The programs the run started, by file name, with their digests, in the order they started.
fn programs(model: &RunModel) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    for record in model.binaries.iter().filter(|r| r.role != "library" && r.role != "loader") {
        let name = record.path.rsplit('/').next().unwrap_or(&record.path).to_owned();
        if !found.iter().any(|(seen, _)| *seen == name) {
            found.push((name, record.blake3.clone()));
        }
    }
    found
}

fn target(out: &mut String, programs: &[(String, String)], source: Option<&Source>) {
    out.push_str("## Target\n\n| | |\n|---|---|\n");
    if programs.is_empty() {
        out.push_str("| Program | not recorded in the bundle |\n");
    }
    for (name, digest) in programs {
        let _ = writeln!(out, "| Program | `{name}`, blake3 `{digest}` |");
    }
    match source {
        Some(source) => {
            let _ = writeln!(
                out,
                "| Source | {} at `{}` (target pack `{}`) |",
                source.repository, source.commit, source.pack
            );
        }
        None => out.push_str(
            "| Source | not recorded: no target pack names this build's digest, so the build is \
             identified by its digest alone |\n",
        ),
    }
    out.push('\n');
}

fn found(out: &mut String, model: &RunModel, story: &Story) {
    let verdict = &story.verdict;
    out.push_str("## What went wrong\n\n");
    let point = story.steps.iter().find(|step| step.violation);
    match point {
        Some(step) => {
            let _ = writeln!(
                out,
                "{} (`{}`), at event {} on node {}, {} into the run.\n",
                capitalised(&verdict.headline),
                verdict.code,
                step.event,
                step.node,
                narrate::seconds(step.tick)
            );
        }
        None => {
            let _ = writeln!(out, "{} (`{}`).\n", capitalised(&verdict.headline), verdict.code);
        }
    }
    if let Some(location) = model.violation.as_ref().and_then(|v| v.location.as_ref()) {
        let _ = writeln!(
            out,
            "The promise is written at `{}:{}`, in `{}`.\n",
            location.file, location.line, location.function
        );
    }
    let detail = describe::outcome_detail(&model.manifest.outcome).unwrap_or("");
    let also: Vec<&str> = detail.split("; ").skip(1).collect();
    if !also.is_empty() {
        out.push_str("The run also recorded:\n\n");
        for line in also {
            let _ = writeln!(out, "- {line}");
        }
        out.push('\n');
    }

    if story.steps.is_empty() {
        return;
    }
    out.push_str(
        "### Before it\n\n| # | Event | Time | Node | What happened |\n|---:|---:|---:|---:|---|\n",
    );
    if story.elided > 0 {
        let _ = writeln!(out, "| | | | | {} earlier steps not shown |", story.elided);
    }
    for (i, step) in story.steps.iter().enumerate() {
        let mark = if step.violation { " **← the violation**" } else { "" };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {}{mark} |",
            i + 1,
            step.event,
            narrate::seconds(step.tick),
            step.node,
            cell(&step.text)
        );
    }
    out.push_str(
        "\nThese are the faults, node changes and failures recorded before the violation, \
         gathered by rule. That they caused it is not shown, only that they came first.\n\n",
    );
}

fn reproduce(out: &mut String, model: &RunModel, bundle: &str) {
    let manifest = &model.manifest;
    let _ = writeln!(
        out,
        "## How to reproduce\n\n```sh\nbohr replay {}\n```\n\n\
         The replay re-executes the recorded programs and compares what they do, event by event, \
         with the trace digest the run recorded:\n\n```\n{}\n```\n\n\
         A replay that reaches any other digest says so and exits non-zero. It needs a bohr \
         build that reads tape ABI {}, a Linux x86-64 host for a program on disk, and the \
         programs named under Target: one whose blake3 differs is refused. `bohr explain {}` \
         tells the run step by step.\n",
        quoted(bundle),
        manifest.trace_digest,
        manifest.tape_abi,
        quoted(bundle)
    );
}

fn run(out: &mut String, model: &RunModel, story: &Story) {
    let manifest = &model.manifest;
    let provenance = &manifest.provenance;
    let _ = writeln!(
        out,
        "## What was run\n\n- {}\n- scheduler `{}`, world model `{}`, oracle `{}`, backend `{}`\n\
         - determinism: {}\n",
        story.run,
        manifest.policy.scheduler,
        provenance.world_model,
        provenance.oracle,
        provenance.backend,
        manifest.determinism.level
    );
    if let Some(from) = &manifest.reduced_from {
        let _ = writeln!(out, "Reduced from run `{from}` by `bohr reduce`.\n");
    }
    if model.config.trim().is_empty() {
        out.push_str("No configuration: the program ran on one node, with no faults.\n\n");
    } else {
        let fence = fence_for(&model.config);
        let _ = writeln!(
            out,
            "The configuration, as it was written:\n\n{fence}toml\n{}\n{fence}\n",
            model.config.trim_end()
        );
    }
}

fn limits(out: &mut String, model: &RunModel) {
    out.push_str(
        "## What this cannot claim\n\n\
         - **That a real kernel does this.** Every call the program made was answered by bohr's \
         model of Linux, not by a kernel. A call the model answers differently from Linux is a \
         difference this run cannot see.\n\
         - **That it happens in production, or how often.** One seed shows the failure is \
         reachable under this model; it says nothing about its rate on real machines.\n\
         - **Why.** The steps above came before the violation. Nothing here shows they caused it.\n",
    );
    if model.events.iter().any(|e| matches!(e.kind, EventKind::FaultFired { .. })) {
        out.push_str(
            "- **That the faults are the ones hardware produces.** Crashes, losses and errors were \
             injected where the model chose; a crash loses exactly what the model says was not yet \
             durable.\n",
        );
    }
    for reason in &model.manifest.determinism.reasons {
        let _ = writeln!(out, "- **Determinism was degraded:** {reason}");
    }
    out.push('\n');
}

/// A fence longer than any run of backticks in `text`, so the text cannot close it.
fn fence_for(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

/// A path as a shell reads it back: bare where it is plain, single-quoted otherwise.
fn quoted(path: &str) -> String {
    let plain = !path.is_empty()
        && path.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._/+:@".contains(&b));
    if plain { path.to_owned() } else { format!("'{}'", path.replace('\'', "'\\''")) }
}

/// The first letter in upper case, for a headline that starts a sentence.
fn capitalised(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}

/// Text that cannot break a table row.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cell_cannot_end_its_row() {
        assert_eq!(cell("a | b\nc"), "a \\| b c");
    }

    #[test]
    fn a_configuration_cannot_close_its_own_fence() {
        assert_eq!(fence_for("a = 1"), "```");
        assert_eq!(fence_for("note = \"```\""), "````");
    }

    #[test]
    fn a_path_is_quoted_only_where_a_shell_would_split_it() {
        assert_eq!(quoted("findings/a-1.bohr"), "findings/a-1.bohr");
        assert_eq!(quoted("my run.bohr"), "'my run.bohr'");
        assert_eq!(quoted("it's.bohr"), "'it'\\''s.bohr'");
    }

    #[test]
    fn a_headline_starts_a_sentence() {
        assert_eq!(capitalised("a guest failed"), "A guest failed");
        assert_eq!(capitalised(""), "");
    }
}
