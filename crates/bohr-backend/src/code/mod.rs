//! What machine code is, as far as a supervisor has to know.
//!
//! Two questions and no more: where does an instruction end, and where does a function begin.
//! Neither is about what a program means. They are here rather than in the interceptor because the
//! answers are needed on both sides of the boundary — by the diagnosis that reports what a target
//! carries, and by the world that decides what a guest is allowed to execute.

pub mod cpuid;
pub mod entropy;
pub mod insn;
pub mod sites;
pub mod symbols;
