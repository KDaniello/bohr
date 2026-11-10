//! A deliberate defect: a campaign that folds results as they arrive.
//!
//! It shows the test catches it. Only the feature of the same name enables it; see
//! `docs/TESTING.md` §7.4.

#[cfg(feature = "chaos-worker-order")]
use crate::campaign::RunResult;

/// Leaves the results in whatever order the workers finished in.
///
/// One worker and eight then explore different corpora from one master seed.
#[cfg(feature = "chaos-worker-order")]
pub(crate) fn in_arrival_order(_results: &mut [RunResult]) {}
