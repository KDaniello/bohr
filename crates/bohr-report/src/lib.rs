//! Turning a stored bundle into something a person reads.
//!
//! Sans-io like the layers below it: nothing here opens a file, spawns anything or reads a clock.
//! A renderer is handed a parsed bundle and answers with text — which is what lets a report be
//! byte-for-byte the same twice, and what makes the output testable without a filesystem.
//!
//! The audience is the reason the crate exists. A bundle is complete and unreadable: sixteen kinds
//! of event, eighteen draw sites, a manifest of contract numbers. Somebody arriving from a bug
//! report has none of that vocabulary and one question — what went wrong, and what should I do
//! next. Every decision here is answerable from that sentence.

#![forbid(unsafe_code)]

pub mod describe;
mod escape;
mod html;
pub mod jsonl;
mod model;
pub mod text;
mod window;

/// Naming a syscall, from the one table this workspace holds. Re-exported rather than copied:
/// the crate below already carries it, because a digest has to know what a call is called
/// with.
pub use bohr_trace::{named_syscalls, syscall_name};
pub use model::{Location, RunModel, Violation};
pub use text::summary;

/// The largest page this crate will produce.
///
/// A bound rather than a preference: a report exists to be attached to an issue and opened by
/// somebody who did not ask for it, and a file too large for either has stopped being a report.
/// A run too long to draw is windowed rather than truncated, and what is left out is written
/// beside it — see [`Rendered::sidecar`].
pub const MOST_BYTES: usize = 4 * 1024 * 1024;

/// A rendered report, and whatever has to travel with it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Rendered {
    /// The page.
    pub html: String,
    /// Every event of the run, one per line, where the page did not draw them all.
    ///
    /// Present exactly when the page is partial. A reader who has to reconcile a page with a file
    /// in a different shape has been given two problems, so this is the same projection
    /// `bohr trace --format json` writes.
    pub sidecar: Option<String>,
}

/// Renders a report for a stored run.
///
/// `sidecar_name` is what the page calls the file beside it. Naming rather than writing: this
/// crate does not open files, and what a caller does with the two strings is the caller's.
///
/// # Errors
///
/// Only where the bundle lacks what any reader needs — a manifest that is not readable.
pub fn render(
    bundle: &bohr_trace::Bundle,
    sidecar_name: Option<&str>,
) -> Result<Rendered, ReportError> {
    render_from(bundle, sidecar_name, None)
}

/// The same, told what the bundle it was made from is called.
///
/// **The page prints a command a maintainer is meant to copy, and it named a file nobody writes.**
/// It was built out of the run identifier -- `bohr replay 4f2a1b8c9d0e.bohr` -- while findings are
/// saved as `finding-…`, and a saved run or a reduction takes whatever name its caller chose. So
/// the one line the page exists to hand somebody answered "cannot read". Where the caller knows
/// the name, it says it; where it does not, the line asks for the bundle rather than inventing
/// one.
///
/// # Errors
///
/// As [`render`].
pub fn render_from(
    bundle: &bohr_trace::Bundle,
    sidecar_name: Option<&str>,
    bundle_name: Option<&str>,
) -> Result<Rendered, ReportError> {
    let model = RunModel::of(bundle)?;
    // One criterion, read once. The sidecar was written on the event count and the page said so on
    // whether the window elided anything, so a run whose keep-set covered everything got a file
    // the page never mentioned.
    let rendered = html::render_with(&model, sidecar_name, bundle_name);
    let sidecar = rendered.partial.then(|| jsonl::write_log(&model.events));
    Ok(Rendered { html: rendered.html, sidecar })
}

/// Renders a report for a model somebody already has.
///
/// The seam a test uses to put a hostile guest's own words into a page and check they stay words.
#[must_use]
pub fn render_model(model: &RunModel, sidecar_name: Option<&str>) -> String {
    html::render(model, sidecar_name)
}

/// Why a bundle could not be turned into a report.
#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    /// The bundle is not one, or its manifest is not readable.
    #[error("{0}")]
    Unreadable(#[from] bohr_trace::TraceError),
}
