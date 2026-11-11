//! Builds bohr's vDSO from `vdso/vdso.c`, for a Linux x86-64 target, into `OUT_DIR`.
//!
//! The image is built from committed source every time, never committed as bytes, as the guests
//! are. Freestanding and position independent, so the C compiler that builds any guest builds it;
//! on another target the image is empty and the backend withholds the vDSO as before.

use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=vdso/vdso.c");
    println!("cargo:rerun-if-changed=vdso/vdso.lds");
    println!("cargo:rerun-if-changed=vdso/layout.lds");
    println!("cargo:rerun-if-env-changed=BOHR_VDSO_CC");
    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("bohr-vdso.so");
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if os != "linux" || arch != "x86_64" {
        fs::write(&out, []).expect("OUT_DIR is writable");
        return;
    }
    let cc = env::var("BOHR_VDSO_CC").unwrap_or_else(|_| "cc".to_owned());
    let status = Command::new(&cc)
        .args([
            "-O2",
            "-fPIC",
            "-ffreestanding",
            "-fno-stack-protector",
            "-fno-asynchronous-unwind-tables",
            "-fno-jump-tables",
            "-nostdlib",
            "-shared",
            "-Wl,-soname=linux-vdso.so.1",
            "-Wl,--hash-style=both",
            "-Wl,--version-script=vdso/vdso.lds",
            "-Wl,-T,vdso/layout.lds",
            "-Wl,-z,max-page-size=4096",
            "-Wl,-z,norelro",
            "-Wl,--no-undefined",
            "-Wl,-Bsymbolic",
            "-o",
        ])
        .arg(&out)
        .arg("vdso/vdso.c")
        .status()
        .unwrap_or_else(|e| panic!("bohr's vDSO needs a C compiler, `{cc}`: {e}"));
    assert!(status.success(), "building bohr's vDSO with `{cc}` failed: {status}");
}
