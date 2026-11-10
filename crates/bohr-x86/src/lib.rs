//! Machine code, as far as a supervisor needs it: where an instruction ends, where a function
//! begins, and the instructions that ask the processor for a number or for what it is.
//!
//! A leaf: bytes in, sites and rewritten copies out, with no dependency and no I/O. The backend's
//! diagnosis, the world's prepared state and the command line's rewritten programs all use it.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cpuid;
pub mod entropy;
pub mod insn;
pub mod sites;
pub mod symbols;
