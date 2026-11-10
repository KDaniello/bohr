//! Machine code, as far as a supervisor needs it: where an instruction ends and where a function
//! begins.
//!
//! It lives here because both the diagnosis and the world use it.

pub mod cpuid;
pub mod entropy;
pub mod insn;
pub mod sites;
pub mod symbols;
