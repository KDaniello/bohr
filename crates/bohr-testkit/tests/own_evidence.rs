//! This world refuses a run in which it has shown the guest something no machine could.
//!
//! **The most expensive thing about hunting a real target was not finding defects; it was telling
//! whose they were.** Of four things that looked like an execution client's bugs on the first day
//! against one, three were this tool's, and each was disproved only by reading the client's source
//! — which does not scale, and is the opposite of what an external simulator is for.
//!
//! Two rules close two of those three classes, and they are stated as refusals rather than as
//! findings: a guest that was told something untrue about its own state is not evidence about the
//! guest, so the run stops under `BOHR-5005` instead of being reported.
//!
//! **A rule that cannot be seen failing is not a rule.** Each is tested against the defect it
//! exists for, reintroduced by the injection that names it, exactly as `docs/TESTING.md` §7.4 has
//! every other determinism claim tested. Without the injection this file compiles to nothing:
//! there is no way to ask the question of a build that has the defect fixed.

#![cfg(any(feature = "chaos-unknown-descriptor", feature = "chaos-proc-on-the-disk"))]

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullProgram};
use bohr_testkit::{Preload, Scenario, refusable_scenario};
use bohr_types::Seed;

const PAYLOAD_AT: u64 = MEMORY_BASE + 0x200;

/// What a refused run says, whichever rule refused it.
const REFUSAL: &str = "BOHR-5005";

/// The refusal a scenario produced, or what it did instead.
fn refusal(programs: Vec<NullProgram>, preloads: &[Preload], scenario: Scenario) -> String {
    match refusable_scenario(programs, preloads, scenario, Seed(1)) {
        Err(refused) => refused.to_string(),
        Ok(_) => String::from("the run completed"),
    }
}

#[cfg(feature = "chaos-unknown-descriptor")]
mod descriptor {
    use bohr_types::Tick;
    use bohr_world::NetworkConfig;

    use super::*;

    /// `writev`.
    const WRITEV: u32 = 20;
    /// `socket`.
    const SOCKET: u32 = 41;
    /// `bind`.
    const BIND: u32 = 49;
    /// `nanosleep`.
    const SLEEP: u32 = 35;

    /// `AF_INET`, `SOCK_DGRAM`.
    const INET_DGRAM: [u64; 6] = [2, 2, 0, 0, 0, 0];

    const SOCKADDR_AT: u64 = MEMORY_BASE + 0x300;
    const IOVEC_AT: u64 = MEMORY_BASE + 0x400;

    /// The first descriptor a socket is given.
    const SOCKET_FD: u64 = 4096;

    /// Opens a socket and answers on it in one vectored call, as a server writing headers and a
    /// body does.
    fn answers_in_one_call() -> NullProgram {
        NullProgram::single(vec![
            GuestOp::Syscall { nr: SOCKET, args: INET_DGRAM },
            GuestOp::Syscall { nr: BIND, args: [SOCKET_FD, SOCKADDR_AT, 16, 0, 0, 0] },
            GuestOp::Syscall { nr: WRITEV, args: [SOCKET_FD, IOVEC_AT, 1, 0, 0, 0] },
            GuestOp::Syscall { nr: SLEEP, args: [MEMORY_BASE + 0x100, 0, 0, 0, 0, 0] },
        ])
    }

    fn memory() -> Vec<Preload> {
        let mut sockaddr = vec![2, 0, 0x27, 0x0f];
        sockaddr.extend_from_slice(&[10, 66, 0, 0]);
        sockaddr.extend_from_slice(&[0u8; 8]);
        let mut iovec = PAYLOAD_AT.to_le_bytes().to_vec();
        iovec.extend_from_slice(&8u64.to_le_bytes());
        let mut timespec = 0u64.to_le_bytes().to_vec();
        timespec.extend_from_slice(&Tick::MILLISECOND.get().to_le_bytes());
        vec![
            (0, MEMORY_BASE + 0x100, timespec),
            (0, SOCKADDR_AT, sockaddr),
            (0, IOVEC_AT, iovec),
            (0, PAYLOAD_AT, b"HTTP/1.1".to_vec()),
        ]
    }

    /// The claim: a socket answered as though this world had never issued it stops the run.
    ///
    /// The defect, verbatim: `writev` was left off the calls that follow a socket descriptor, so a
    /// vectored answer was refused with `EBADF` for a socket the guest was holding. What the
    /// client did afterwards read as a client defect for a day.
    #[test]
    fn a_descriptor_this_world_issued_is_never_answered_as_unknown() {
        let scenario = Scenario {
            network: NetworkConfig { nodes: 1, ..NetworkConfig::default() },
            ..Scenario::default()
        };
        let said = refusal(vec![answers_in_one_call()], &memory(), scenario);
        assert!(
            said.contains(REFUSAL) && said.contains("EBADF"),
            "a socket was answered as unknown and the run went on to say: {said}"
        );
    }
}

#[cfg(feature = "chaos-proc-on-the-disk")]
mod disk {
    use bohr_types::{NodeId, Tick};
    use bohr_world::{NetworkConfig, NodeFaultKind, ScheduledNodeFault};

    use super::*;

    /// `openat`.
    const OPENAT: u32 = 257;
    /// `read`.
    const READ: u32 = 0;
    /// `write`.
    const WRITE: u32 = 1;
    /// `fsync`.
    const FSYNC: u32 = 74;
    /// `nanosleep`.
    const SLEEP: u32 = 35;

    /// `O_CREAT | O_RDWR`.
    const OPEN_FLAGS: u64 = 0o100 | 2;

    const PROC_PATH_AT: u64 = MEMORY_BASE;
    const DATA_PATH_AT: u64 = MEMORY_BASE + 0x080;
    const TIMESPEC_AT: u64 = MEMORY_BASE + 0x100;
    const INBOX_AT: u64 = MEMORY_BASE + 0x180;

    const PROC_FD: u64 = 3;
    const DATA_FD: u64 = 4;

    /// Asks who it is, makes an ordinary file durable, and waits to be killed.
    fn asks_who_it_is() -> NullProgram {
        let mut ops = vec![
            GuestOp::Syscall { nr: OPENAT, args: [u64::MAX, PROC_PATH_AT, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: READ, args: [PROC_FD, INBOX_AT, 256, 0, 0, 0] },
            GuestOp::Syscall {
                nr: OPENAT,
                args: [u64::MAX, DATA_PATH_AT, OPEN_FLAGS, 0o644, 0, 0],
            },
            GuestOp::Syscall { nr: WRITE, args: [DATA_FD, PAYLOAD_AT, 64, 0, 0, 0] },
            GuestOp::Syscall { nr: FSYNC, args: [DATA_FD, 0, 0, 0, 0, 0] },
        ];
        ops.extend(
            (0..40).map(|_| GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] }),
        );
        NullProgram::single(ops)
    }

    fn memory() -> Vec<Preload> {
        let mut timespec = 0u64.to_le_bytes().to_vec();
        timespec.extend_from_slice(&Tick::MILLISECOND.get().to_le_bytes());
        vec![
            (0, PROC_PATH_AT, b"/proc/self/stat\0".to_vec()),
            (0, DATA_PATH_AT, b"/data\0".to_vec()),
            (0, TIMESPEC_AT, timespec),
            (0, PAYLOAD_AT, vec![b'x'; 64]),
        ]
    }

    /// The claim: a crash that would roll back what the kernel answers stops the run.
    ///
    /// The defect, verbatim: generated `/proc` files were on the modelled disk, so a crash took
    /// away the process's own entry and an execution client panicked on `expect("own process")`
    /// after coming back. It was one syscall away from being filed upstream.
    #[test]
    fn a_disk_never_holds_what_the_kernel_answers_out_of_itself() {
        let scenario = Scenario {
            network: NetworkConfig { nodes: 1, ..NetworkConfig::default() },
            node_faults: vec![ScheduledNodeFault {
                at_tick: Tick(8 * Tick::MILLISECOND.get()),
                node: NodeId::ZERO,
                kind: NodeFaultKind::Kill,
            }],
            ..Scenario::default()
        };
        let said = refusal(vec![asks_who_it_is()], &memory(), scenario);
        assert!(
            said.contains(REFUSAL) && said.contains("kernel"),
            "a crash rolled back what the kernel answers and the run went on to say: {said}"
        );
    }
}
