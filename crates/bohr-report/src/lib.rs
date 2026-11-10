//! Turning a stored bundle into something a person reads.
//!
//! Sans-io: a renderer takes a parsed bundle and returns text, so a report is the same bytes every
//! time and is tested without a filesystem.
//!
//! Written for someone arriving from a bug report who asks what went wrong and what to do next.

#![forbid(unsafe_code)]

pub mod chain;
pub mod describe;
pub mod disclosure;
mod escape;
mod html;
pub mod jsonl;
mod model;
pub mod narrate;
pub mod sarif;
pub mod text;
mod window;

/// Syscall names, re-exported from the workspace's one table.
pub use bohr_trace::{named_syscalls, syscall_name};
pub use model::{Location, RunModel, Violation};
pub use text::summary;

/// The largest page this crate will produce.
///
/// Small enough to attach to an issue. A longer run is windowed and the rest goes in
/// [`Rendered::sidecar`].
pub const MOST_BYTES: usize = 4 * 1024 * 1024;

/// A rendered report, and whatever has to travel with it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Rendered {
    /// The page.
    pub html: String,
    /// Every event of the run, one per line, where the page did not draw them all.
    ///
    /// Present exactly when the page is partial, in the format `bohr trace --format json` writes.
    pub sidecar: Option<String>,
}

/// Renders a report for a stored run.
///
/// `sidecar_name` is what the page calls the file beside it; the caller writes both.
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
/// The reproduction command names this bundle; without a name it shows a placeholder.
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
    // One criterion decides both the sidecar and the page's mention of it.
    let rendered = html::render_with(&model, sidecar_name, bundle_name, &[]);
    let sidecar = rendered.partial.then(|| jsonl::write_log(&model.events));
    Ok(Rendered { html: rendered.html, sidecar })
}

/// The same, with the lanes the chain's timeline is drawn from.
///
/// The caller derives the lanes from the scenario, which this crate does not read.
///
/// # Errors
///
/// As [`render`].
pub fn render_chain(
    bundle: &bohr_trace::Bundle,
    sidecar_name: Option<&str>,
    bundle_name: Option<&str>,
    lanes: &[chain::Lane],
) -> Result<Rendered, ReportError> {
    let model = RunModel::of(bundle)?;
    let rendered = html::render_with(&model, sidecar_name, bundle_name, lanes);
    let sidecar = rendered.partial.then(|| jsonl::write_log(&model.events));
    Ok(Rendered { html: rendered.html, sidecar })
}

/// Renders a report for a model somebody already has.
///
/// Lets a test render hostile guest strings and check they stay text.
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
