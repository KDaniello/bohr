//! A deliberate defect: a campaign that folds results as they arrive.
//!
//! Present so the test that is supposed to catch it can be shown to catch it. Reachable only
//! through the feature that names it, which no ordinary build enables. See `docs/TESTING.md` §7.4.

#[cfg(feature = "chaos-worker-order")]
use crate::campaign::RunResult;

/// Leaves the results in whatever order the workers finished in.
///
/// The shape of the mistake it stands for: any campaign that lets arrival order reach a decision.
/// One worker and eight then explore different corpora from the same master seed, and a finding
/// from the eighth cannot be reproduced by anyone running one.
#[cfg(feature = "chaos-worker-order")]
pub(crate) fn in_arrival_order(_results: &mut [RunResult]) {}
