//! Links the edge-counter runtime the supervisor reads a search's coverage from.
//!
//! The guest is built with `-fsanitize-coverage=trace-pc-guard` (see `rustflags.txt`), which calls
//! `__sanitizer_cov_trace_pc_guard` at every edge. `sdk/c/bohr-cov-rt.c` is compiled here, so guest
//! and supervisor agree on the map size.
//!
//! One `cc` call, without a build dependency.

use std::{path::PathBuf, process::Command};

fn main() {
    let runtime = PathBuf::from("../../sdk/c/bohr-cov-rt.c");
    println!("cargo:rerun-if-changed={}", runtime.display());

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let object = out.join("bohr-cov-rt.o");

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    // Built without instrumentation, or the edge recorder would recurse.
    let status = Command::new(&cc)
        .args(["-O1", "-fno-stack-protector", "-c", "-o"])
        .arg(&object)
        .arg(&runtime)
        .status()
        .unwrap_or_else(|e| panic!("cannot run {cc}: {e}"));
    assert!(status.success(), "{cc} could not compile the coverage runtime");

    println!("cargo:rustc-link-arg={}", object.display());
}
