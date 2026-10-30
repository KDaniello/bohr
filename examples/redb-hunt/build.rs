//! Links the edge-counter runtime the supervisor reads a search's coverage from.
//!
//! The guest is compiled with `-fsanitize-coverage=trace-pc-guard` (see `rustflags.txt` beside
//! this file), which makes the compiler call `__sanitizer_cov_trace_pc_guard` at every edge. The
//! runtime that answers those calls is `sdk/c/bohr-cov-rt.c`, and it is compiled here rather than
//! vendored so that the guest and the supervisor can never disagree about the map's size.
//!
//! Built by hand rather than through a crate that does this for us: the tool's own dependency
//! graph stays narrow, and one `cc` invocation is not worth a build dependency.

use std::{path::PathBuf, process::Command};

fn main() {
    let runtime = PathBuf::from("../../sdk/c/bohr-cov-rt.c");
    println!("cargo:rerun-if-changed={}", runtime.display());

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let object = out.join("bohr-cov-rt.o");

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    // Without the instrumentation it implements: built with it, the function that records an edge
    // records the edges inside itself, and the first edge of the program is unbounded recursion.
    let status = Command::new(&cc)
        .args(["-O1", "-fno-stack-protector", "-c", "-o"])
        .arg(&object)
        .arg(&runtime)
        .status()
        .unwrap_or_else(|e| panic!("cannot run {cc}: {e}"));
    assert!(status.success(), "{cc} could not compile the coverage runtime");

    println!("cargo:rustc-link-arg={}", object.display());
}
