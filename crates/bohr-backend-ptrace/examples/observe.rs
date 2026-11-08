//! Prints every stop a guest produces, admitting each syscall unchanged.
//!
//! The support table grows from what real programs actually do, so this is how a new guest's
//! requirements are read off rather than guessed at.
//!
//! `cargo run -p bohr-backend-ptrace --example observe -- ./testdata/guests/build/clock_and_random`

fn main() {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    linux::main();

    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    eprintln!("this example needs Linux on x86-64");
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux {
    use bohr_backend::{DeterminismBackend, GuestSpec, Resume, StopKind};
    use bohr_backend_ptrace::{PtraceBackend, syscall_name};
    use bohr_types::NodeId;

    pub(super) fn main() {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        if argv.is_empty() {
            eprintln!("usage: observe <program> [args...]");
            std::process::exit(2);
        }

        let mut backend = PtraceBackend::new();
        let caps = backend.capabilities();
        println!(
            "backend {} · traps_cpuid={} · traps_rdtsc={}",
            caps.name, caps.traps_cpuid, caps.traps_rdtsc
        );

        let spec = GuestSpec { argv, env: Vec::new() };
        let task = match backend.spawn(NodeId::ZERO, &spec) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("spawn failed: {e}");
                std::process::exit(1);
            }
        };
        if !backend.degradations().is_empty() {
            println!("degraded: {:?}", backend.degradations());
        }

        let mut counts: std::collections::BTreeMap<u32, usize> = std::collections::BTreeMap::new();
        let mut stops = 0usize;

        backend.resume(task, Resume::Continue).expect("first resume");
        while let Some(stop) = backend.next_stop().expect("stop") {
            stops += 1;
            match stop.kind {
                StopKind::SyscallEntry { nr, args } => {
                    *counts.entry(nr).or_default() += 1;
                    println!(
                        "{:>4} syscall {:<4} {:<16} args {:#x} {:#x} {:#x}",
                        stops,
                        nr,
                        syscall_name(nr).unwrap_or("?"),
                        args[0],
                        args[1],
                        args[2]
                    );
                    // Admitted unchanged: the point here is to see the sequence, not to model it.
                    backend.resume(stop.task, Resume::Continue).expect("resume");
                }
                StopKind::TaskExited { status } => {
                    println!("{stops:>4} exited {status:?}");
                    break;
                }
                other => {
                    println!("{stops:>4} {other:?}");
                    backend.resume(stop.task, Resume::Continue).expect("resume");
                }
            }
        }

        println!("\ntraced syscalls, by number:");
        for (nr, n) in counts {
            println!("  {:<4} {:<16} x{}", nr, syscall_name(nr).unwrap_or("UNLISTED"), n);
        }
    }
}
