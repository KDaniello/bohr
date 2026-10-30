//! The suite the whole product rests on: the same seed produces the same run.
//!
//! Every assertion here is also a negative test of the mutation harness, which enables one
//! deliberate defect at a time and requires this file to fail. A test that passes under an
//! injection has a hole in it, and the hole is closed by strengthening the test.

use bohr_testkit::{digests, run_once, Fixture};
use bohr_types::Seed;

/// Repetitions per fixture. Enough that an ordering that only sometimes differs is caught, and
/// cheap enough to run on every commit.
const REPEATS: usize = 200;

#[test]
fn one_seed_one_run() {
    for fixture in Fixture::all() {
        let seed = Seed(0x5EED);
        let seen = digests(fixture, seed, REPEATS);
        let first = seen[0];
        for (i, digest) in seen.iter().enumerate() {
            assert_eq!(*digest, first, "{} diverged on repetition {}", fixture.name(), i + 1);
        }
    }
}

#[test]
fn different_seeds_different_runs() {
    for fixture in Fixture::all() {
        let a = run_once(fixture, Seed(1)).digest;
        let b = run_once(fixture, Seed(2)).digest;
        assert_ne!(a, b, "{} ignores its seed", fixture.name());
    }
}

#[test]
fn the_whole_run_repeats_not_only_its_digest() {
    for fixture in Fixture::all() {
        let first = run_once(fixture, Seed(7));
        let again = run_once(fixture, Seed(7));
        assert_eq!(first.events, again.events, "{}: event count", fixture.name());
        assert_eq!(first.ticks, again.ticks, "{}: virtual time", fixture.name());
        assert_eq!(first.journal.records(), again.journal.records(), "{}: draws", fixture.name());
    }
}

/// A run driven by a longer tape must read the same bytes at the same offsets as a shorter one,
/// because consumption is a function of the request alone.
#[test]
fn draws_land_at_the_same_offsets() {
    for fixture in Fixture::all() {
        let first = run_once(fixture, Seed(11));
        let again = run_once(fixture, Seed(11));
        let offsets: Vec<_> = first.journal.records().iter().map(|r| r.offset).collect();
        let repeated: Vec<_> = again.journal.records().iter().map(|r| r.offset).collect();
        assert_eq!(offsets, repeated, "{}: draw offsets", fixture.name());
    }
}

/// Every draw carries the event it was made for, which is what lets the journal be read against the
/// trace. A journal whose entries all name event zero says nothing about when a decision was taken.
#[test]
fn draws_name_the_event_they_belong_to() {
    let report = run_once(Fixture::ClockAndRandom, Seed(3));
    let events: Vec<u64> = report.journal.records().iter().map(|r| r.event).collect();
    assert!(!events.is_empty(), "the fixture must draw something");
    assert!(events.iter().any(|e| *e > 0), "every draw was filed under event zero: {events:?}");
    assert!(
        events.windows(2).all(|w| w[0] <= w[1]),
        "draws must be filed in the order events were recorded: {events:?}"
    );
    assert!(
        events.iter().all(|e| *e < report.events),
        "a draw cannot belong to an event that was never recorded"
    );
}

/// Which waiter a wake releases comes from the tape, not from the order they arrived in.
///
/// This is the promise `docs/DETERMINISM.md` makes about `futex`: the kernel may wake waiters in
/// any order, so the model keeps its own queue and draws the choice. A run that took the first
/// waiter would still reproduce — and would explore one ordering forever.
#[test]
fn futex_wake_order_comes_from_the_tape() {
    let orders: Vec<Vec<u64>> = (0..32u64)
        .map(|seed| {
            let report = run_once(Fixture::TwoWaiters, Seed(seed));
            report
                .journal
                .records()
                .iter()
                .filter(|r| r.site == bohr_tape::DrawSite::SyncWake as u16)
                .map(|r| r.value)
                .collect()
        })
        .collect();

    assert!(orders.iter().any(|o| !o.is_empty()), "no wake ever consulted the tape");
    assert!(
        orders.iter().any(|o| *o != orders[0]),
        "every seed released the waiters in the same order: {:?}",
        orders[0]
    );
}

mod cluster {
    use bohr_backend_null::{GuestOp, NullProgram, MEMORY_BASE};
    use bohr_testkit::{distributed_identity, run_cluster, world_identity};
    use bohr_trace::{EventKind, Outcome};
    use bohr_types::Seed;

    const CLOCK_GETTIME: u32 = 228;
    const FUTEX: u32 = 202;
    const FUTEX_WAIT: u64 = 0;
    const FUTEX_WAKE: u64 = 1;

    fn read_clock() -> NullProgram {
        NullProgram::single(vec![
            GuestOp::Syscall { nr: CLOCK_GETTIME, args: [0, MEMORY_BASE, 0, 0, 0, 0] },
            GuestOp::Exit(0),
        ])
    }

    /// The presented time carries each node's own offset, which is millions of ticks; a cluster
    /// answered from the world clock alone would read small multiples of the syscall cost. This
    /// is the test the shared-clock injection must fail.
    #[test]
    fn node_clocks_are_their_own_not_the_worlds() {
        let (_, events) = run_cluster(vec![read_clock(), read_clock()], &[], Seed(5));
        let mut first_read = std::collections::BTreeMap::new();
        for event in &events {
            if let EventKind::ClockRead { presented } = event.kind {
                first_read.entry(event.node).or_insert(presented.get());
            }
        }
        assert_eq!(first_read.len(), 2, "both nodes read the clock");
        let loudest = first_read.values().max().copied().unwrap_or(0);
        assert!(
            loudest > 1_000_000,
            "no node shows its drawn offset; the cluster is reading one shared clock ({loudest})"
        );
    }

    /// The same numeric address on two nodes is two different words. A wake crossing node
    /// boundaries would release the waiter and let this run finish; the honest outcome is a hang.
    #[test]
    fn futex_spaces_do_not_cross_nodes() {
        let word = MEMORY_BASE + 512;
        let waiter = NullProgram::single(vec![
            GuestOp::Syscall { nr: FUTEX, args: [word, FUTEX_WAIT, 0, 0, 0, 0] },
            GuestOp::Exit(0),
        ]);
        let waker = NullProgram::single(vec![
            GuestOp::Syscall { nr: FUTEX, args: [word, FUTEX_WAKE, 1, 0, 0, 0] },
            GuestOp::Exit(0),
        ]);

        let (report, _) = run_cluster(vec![waiter, waker], &[], Seed(9));
        assert!(
            matches!(report.outcome, Outcome::GuestHung(_)),
            "a wake on one node released a waiter on another: {:?}",
            report.outcome
        );
    }

    /// A timed wait that nothing wakes comes back with a timeout instead of hanging the run.
    #[test]
    fn a_timed_wait_expires_instead_of_deadlocking() {
        let timeout_at = MEMORY_BASE + 256;
        let two_ms: u64 = 2_000_000;
        let waiter = NullProgram::single(vec![
            GuestOp::Syscall { nr: FUTEX, args: [MEMORY_BASE, FUTEX_WAIT, 0, timeout_at, 0, 0] },
            GuestOp::Exit(0),
        ]);
        // A timespec of {0 s, 2 ms} at the timeout address.
        let mut timespec = vec![0u8; 16];
        timespec[8..16].copy_from_slice(&two_ms.to_le_bytes());

        let (report, _) = run_cluster(vec![waiter], &[(0, timeout_at, timespec)], Seed(2));
        assert_eq!(report.outcome, Outcome::Clean, "got {:?}", report.outcome);
        assert!(
            report.ticks.get() >= two_ms,
            "the wait must have lasted its timeout, reached {:?}",
            report.ticks
        );
    }

    #[test]
    fn five_nodes_repeat_exactly() {
        let programs = || (0..5).map(|_| read_clock()).collect::<Vec<_>>();
        let (first, _) = run_cluster(programs(), &[], Seed(77));
        for attempt in 0..200 {
            let (again, _) = run_cluster(programs(), &[], Seed(77));
            assert_eq!(first.digest, again.digest, "attempt {attempt} diverged");
        }
    }

    /// §5.16: the two world models are swappable, and a bundle from one refuses the other.
    #[test]
    fn world_models_refuse_each_others_bundles() {
        let oracle = bohr_types::OracleId::new("accept-everything", 1);
        let manifest = bohr_trace::manifest_for(bohr_trace::ManifestInputs {
            reduced_from: None,
            state_digest: None,
            seed: Seed(1),
            trace_digest: bohr_types::Digest([0; 32]),
            run_id: [0; 16],
            provenance: bohr_trace::Provenance {
                world_model: distributed_identity(),
                oracle: oracle.clone(),
                backend: "null".to_owned(),
            },
            outcome: Outcome::Clean,
            outcome_event: None,
            counters: bohr_trace::Counters::default(),
            policy: bohr_trace::PolicyRecord {
                scheduler: "random".to_owned(),
                pct_depth: None,
                switch_points: "syscall+futex".to_owned(),
            },
            degraded_reasons: Vec::new(),
            created_utc: bohr_trace::now_utc(),
        });
        assert!(manifest.check_provenance(&distributed_identity(), &oracle).is_ok());
        assert!(
            manifest.check_provenance(&world_identity(), &oracle).is_err(),
            "a distributed/1 bundle must refuse to replay under minimal/1"
        );
    }
}

mod network {
    use bohr_backend_null::{GuestOp, NullProgram, MEMORY_BASE};
    use bohr_testkit::{run_networked, sockaddr_for};
    use bohr_trace::{EventKind, Outcome, PacketFate};
    use bohr_types::{NodeId, Seed, Tick};
    use bohr_world::{NetworkConfig, ScheduledPartition};

    const SOCKET: u32 = 41;
    const SENDTO: u32 = 44;
    const RECVFROM: u32 = 45;
    const BIND: u32 = 49;
    /// The first socket a node opens; deterministic, which is what lets a script name it.
    const FD: u64 = 4096;

    const ADDR_SELF: u64 = MEMORY_BASE;
    const ADDR_PEER: u64 = MEMORY_BASE + 16;
    const PAYLOAD: u64 = MEMORY_BASE + 64;
    const INBOX: u64 = MEMORY_BASE + 128;

    fn receiver() -> NullProgram {
        NullProgram::single(vec![
            GuestOp::Syscall { nr: SOCKET, args: [2, 2, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: BIND, args: [FD, ADDR_SELF, 16, 0, 0, 0] },
            GuestOp::Syscall { nr: RECVFROM, args: [FD, INBOX, 64, 0, 0, 0] },
            GuestOp::Exit(0),
        ])
    }

    fn sender() -> NullProgram {
        NullProgram::single(vec![
            GuestOp::Syscall { nr: SOCKET, args: [2, 2, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: BIND, args: [FD, ADDR_SELF, 16, 0, 0, 0] },
            GuestOp::Syscall { nr: SENDTO, args: [FD, PAYLOAD, 4, 0, ADDR_PEER, 16] },
            GuestOp::Exit(0),
        ])
    }

    fn wiring(latency: u64) -> NetworkConfig {
        NetworkConfig { nodes: 2, latency_mean_ticks: latency, ..NetworkConfig::default() }
    }

    /// TESTING 4.3: a task blocked in `recvfrom` becomes runnable when a packet is delivered,
    /// and not before.
    #[test]
    fn a_blocked_receive_wakes_only_on_delivery() {
        let latency = 5_000_000;
        let preloads = [
            (0u32, ADDR_SELF, sockaddr_for(0, 9000)),
            (0, ADDR_PEER, sockaddr_for(1, 7000)),
            (0, PAYLOAD, b"ping".to_vec()),
            (1, ADDR_SELF, sockaddr_for(1, 7000)),
        ];
        let (report, events) =
            run_networked(vec![sender(), receiver()], &preloads, wiring(latency), Seed(3));

        assert_eq!(report.outcome, Outcome::Clean, "got {:?}", report.outcome);
        assert!(report.ticks.get() >= latency, "the receive completed before the delivery");

        let sent = events
            .iter()
            .find(|e| matches!(e.kind, EventKind::PacketSent { .. }))
            .expect("the send was recorded");
        let delivered = events
            .iter()
            .find(|e| matches!(e.kind, EventKind::PacketFate { fate: PacketFate::Delivered, .. }))
            .expect("the delivery was recorded");
        assert_eq!(delivered.node, NodeId(1), "the fate lands on the receiver");
        assert!(delivered.tick > sent.tick, "a packet is never delivered before it is sent");
    }

    /// DETERMINISM 2.7: a one-directional cut stops one direction and not the other.
    #[test]
    fn a_directional_partition_cuts_one_way_only() {
        let preloads = [
            (0u32, ADDR_SELF, sockaddr_for(0, 9000)),
            (0, ADDR_PEER, sockaddr_for(1, 7000)),
            (0, PAYLOAD, b"a->b".to_vec()),
            (1, ADDR_SELF, sockaddr_for(1, 7000)),
            (1, ADDR_PEER, sockaddr_for(0, 9000)),
            (1, PAYLOAD, b"b->a".to_vec()),
        ];
        let mut config = wiring(1_000_000);
        config.partitions = vec![ScheduledPartition {
            at_tick: Tick(0),
            heal_at_tick: Tick(u64::MAX),
            cut: vec![(NodeId(0), NodeId(1))],
        }];

        let (report, events) = run_networked(vec![sender(), sender()], &preloads, config, Seed(4));
        assert_eq!(report.outcome, Outcome::Clean, "got {:?}", report.outcome);

        let dropped = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::PacketFate { fate: PacketFate::Dropped, .. }))
            .count();
        let delivered = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::PacketFate { fate: PacketFate::Delivered, .. }))
            .count();
        assert_eq!(dropped, 1, "exactly the cut direction dropped");
        assert_eq!(delivered, 1, "the open direction delivered");
        assert!(
            events.iter().any(|e| matches!(
                e.kind,
                EventKind::FaultFired { fault: bohr_trace::FaultKind::NetPartition, .. }
            )),
            "the cut narrated itself"
        );
    }

    /// A stream pair never touches the network: written bytes come back on the peer end.
    #[test]
    fn a_socketpair_carries_bytes_within_the_node() {
        const SOCKETPAIR: u32 = 53;
        const WRITE: u32 = 1;
        const READ: u32 = 0;
        let program = NullProgram::single(vec![
            GuestOp::Syscall { nr: SOCKETPAIR, args: [1, 1, 0, MEMORY_BASE + 512, 0, 0] },
            GuestOp::Syscall { nr: WRITE, args: [FD, PAYLOAD, 2, 0, 0, 0] },
            GuestOp::Syscall { nr: READ, args: [FD + 1, INBOX, 16, 0, 0, 0] },
            GuestOp::Exit(0),
        ]);
        let preloads = [(0u32, PAYLOAD, b"ok".to_vec())];
        let (report, _) =
            run_networked(vec![program], &preloads, NetworkConfig::default(), Seed(6));
        assert_eq!(report.outcome, Outcome::Clean, "got {:?}", report.outcome);
    }

    /// Loss, duplication and jitter all draw from the tape, so a lossy cluster still repeats.
    #[test]
    fn a_lossy_network_repeats_exactly() {
        let preloads = [
            (0u32, ADDR_SELF, sockaddr_for(0, 9000)),
            (0, ADDR_PEER, sockaddr_for(1, 7000)),
            (0, PAYLOAD, b"spam".to_vec()),
            (1, ADDR_SELF, sockaddr_for(1, 7000)),
        ];
        let send = GuestOp::Syscall { nr: SENDTO, args: [FD, PAYLOAD, 4, 0, ADDR_PEER, 16] };
        let spammer = NullProgram::single(vec![
            GuestOp::Syscall { nr: SOCKET, args: [2, 2, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: BIND, args: [FD, ADDR_SELF, 16, 0, 0, 0] },
            send.clone(),
            send.clone(),
            send.clone(),
            send,
            GuestOp::Exit(0),
        ]);
        let config = || NetworkConfig {
            nodes: 2,
            latency_mean_ticks: 100_000,
            latency_jitter_ticks: 300_000,
            drop_probability: (1, 4),
            duplicate_probability: (1, 4),
            reorder_probability: (1, 3),
            ..NetworkConfig::default()
        };

        let (first, _) =
            run_networked(vec![spammer.clone(), receiver()], &preloads, config(), Seed(11));
        for attempt in 0..200 {
            let (again, _) =
                run_networked(vec![spammer.clone(), receiver()], &preloads, config(), Seed(11));
            assert_eq!(first.digest, again.digest, "attempt {attempt} diverged");
        }
    }

    /// Two cuts raised at one tick fire in `(tick, node, seq)` order; this is the run the
    /// deadline-tie injection must break.
    #[test]
    fn same_tick_timers_fire_in_a_recorded_order() {
        let partitions = vec![
            ScheduledPartition {
                at_tick: Tick(1_000),
                heal_at_tick: Tick(u64::MAX),
                cut: vec![(NodeId(0), NodeId(1))],
            },
            ScheduledPartition {
                at_tick: Tick(1_000),
                heal_at_tick: Tick(u64::MAX),
                cut: vec![(NodeId(1), NodeId(0))],
            },
        ];
        let quiet = || {
            NullProgram::single(vec![
                GuestOp::Syscall { nr: 228, args: [0, MEMORY_BASE, 0, 0, 0, 0] },
                GuestOp::Syscall { nr: 228, args: [0, MEMORY_BASE, 0, 0, 0, 0] },
                GuestOp::Exit(0),
            ])
        };

        let run_one = || {
            let (report, _) = run_networked(
                vec![quiet(), quiet()],
                &[],
                NetworkConfig { partitions: partitions.clone(), ..wiring(0) },
                Seed(13),
            );
            report.digest
        };
        let first = run_one();
        for attempt in 0..200 {
            assert_eq!(first, run_one(), "attempt {attempt} diverged");
        }
    }

    /// A pair without a link is unreachable, and topology is not a fault: the packet drops with
    /// no `FaultFired`.
    #[test]
    fn a_star_reaches_the_hub_and_not_the_rim() {
        let preloads = [
            (1u32, ADDR_SELF, sockaddr_for(1, 9000)),
            (1, ADDR_PEER, sockaddr_for(2, 7000)),
            (1, PAYLOAD, b"rim!".to_vec()),
            (2, ADDR_SELF, sockaddr_for(2, 7000)),
            (2, ADDR_PEER, sockaddr_for(0, 8000)),
            (2, PAYLOAD, b"hub!".to_vec()),
            (0, ADDR_SELF, sockaddr_for(0, 8000)),
        ];
        let quiet_hub = NullProgram::single(vec![
            GuestOp::Syscall { nr: SOCKET, args: [2, 2, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: BIND, args: [FD, ADDR_SELF, 16, 0, 0, 0] },
            GuestOp::Exit(0),
        ]);
        let config = NetworkConfig {
            topology: bohr_world::Topology::Star,
            nodes: 3,
            latency_mean_ticks: 1_000_000,
            ..NetworkConfig::default()
        };

        let (report, events) =
            run_networked(vec![quiet_hub, sender(), sender()], &preloads, config, Seed(8));
        assert_eq!(report.outcome, Outcome::Clean, "got {:?}", report.outcome);

        let dropped = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::PacketFate { fate: PacketFate::Dropped, .. }))
            .count();
        assert_eq!(dropped, 1, "the rim-to-rim send is unreachable");
        assert!(
            !events.iter().any(|e| matches!(e.kind, EventKind::FaultFired { .. })),
            "topology is not a fault"
        );
    }

    /// A grey window drops what the healthy link would carry, and narrates itself.
    #[test]
    fn a_grey_window_degrades_a_link_without_breaking_it() {
        let preloads = [
            (0u32, ADDR_SELF, sockaddr_for(0, 9000)),
            (0, ADDR_PEER, sockaddr_for(1, 7000)),
            (0, PAYLOAD, b"grey".to_vec()),
            (1, ADDR_SELF, sockaddr_for(1, 7000)),
        ];
        let mut config = wiring(1_000_000);
        config.grey = vec![bohr_world::GreyWindow {
            link: (NodeId(0), NodeId(1)),
            at_tick: Tick(0),
            until_tick: Tick(u64::MAX),
            drop_probability: Some((1, 1)),
            latency_factor: None,
        }];

        let (report, events) =
            run_networked(vec![sender(), receiver_that_exits()], &preloads, config, Seed(3));
        assert_eq!(report.outcome, Outcome::Clean, "got {:?}", report.outcome);
        assert!(
            events
                .iter()
                .any(|e| matches!(e.kind, EventKind::PacketFate { fate: PacketFate::Dropped, .. })),
            "a certain-drop window let the packet through"
        );
        assert!(
            events.iter().any(|e| matches!(
                e.kind,
                EventKind::FaultFired { fault: bohr_trace::FaultKind::NetDrop, .. }
            )),
            "the window narrated itself"
        );
    }

    /// The receiver half of the grey test must not block forever on a dropped packet.
    fn receiver_that_exits() -> NullProgram {
        NullProgram::single(vec![
            GuestOp::Syscall { nr: SOCKET, args: [2, 2, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: BIND, args: [FD, ADDR_SELF, 16, 0, 0, 0] },
            GuestOp::Exit(0),
        ])
    }

    /// Several sockets may hold one port where all of them asked for it, and none may otherwise.
    ///
    /// This is how a server spreads one port across its threads, and it is how a validator opens
    /// the four sockets it broadcasts from. Refusing the second bind sent a program that had every
    /// right to make it into a panic, on a conflict no real machine would have reported.
    ///
    /// Both halves, because either alone is a rule that never says no or never says yes.
    #[test]
    fn a_port_is_shared_only_where_everyone_asked_for_it() {
        /// `setsockopt`, `SOL_SOCKET`, `SO_REUSEPORT`.
        const SETSOCKOPT: u32 = 54;
        const SOL_SOCKET: u64 = 1;
        const SO_REUSEPORT: u64 = 15;
        const DATAGRAM: u64 = 2;
        const FD2: u64 = 4097;
        const ON: u64 = MEMORY_BASE + 256;

        let binds = |sharing: bool| {
            let mut ops = vec![GuestOp::Syscall { nr: SOCKET, args: [2, DATAGRAM, 0, 0, 0, 0] }];
            if sharing {
                ops.push(GuestOp::Syscall {
                    nr: SETSOCKOPT,
                    args: [FD, SOL_SOCKET, SO_REUSEPORT, ON, 4, 0],
                });
            }
            ops.push(GuestOp::Syscall { nr: BIND, args: [FD, ADDR_SELF, 16, 0, 0, 0] });
            ops.push(GuestOp::Syscall { nr: SOCKET, args: [2, DATAGRAM, 0, 0, 0, 0] });
            if sharing {
                ops.push(GuestOp::Syscall {
                    nr: SETSOCKOPT,
                    args: [FD2, SOL_SOCKET, SO_REUSEPORT, ON, 4, 0],
                });
            }
            ops.push(GuestOp::Syscall { nr: BIND, args: [FD2, ADDR_SELF, 16, 0, 0, 0] });

            let preloads =
                [(0u32, ADDR_SELF, sockaddr_for(0, 9000)), (0, ON, 1i32.to_le_bytes().to_vec())];
            let (_, events) =
                run_networked(vec![NullProgram::single(ops)], &preloads, wiring(1), Seed(1));
            events
                .iter()
                .filter_map(|event| match &event.kind {
                    EventKind::Syscall { nr, ret, .. } if *nr == BIND => Some(*ret),
                    _ => None,
                })
                .collect::<Vec<i64>>()
        };

        assert_eq!(binds(true), vec![0, 0], "both asked, so both take it");
        assert_eq!(binds(false), vec![0, -98], "neither asked, so the second is refused");
    }

    /// One port number is two endpoints, because Linux gives datagrams and streams a space each.
    ///
    /// A service that binds both for one number is doing the ordinary thing -- gossip on a
    /// validator does exactly that. Keyed together, the second bind of the pair was answered
    /// `EADDRINUSE`, a conflict no real machine would have reported, and the program stopped on
    /// it.
    #[test]
    fn a_datagram_and_a_stream_may_hold_one_port() {
        /// `SOCK_STREAM` and `SOCK_DGRAM`, in the order the call takes them.
        const STREAM: u64 = 1;
        const DATAGRAM: u64 = 2;
        /// The second socket a node opens, the first being `FD`.
        const FD2: u64 = 4097;

        let both = NullProgram::single(vec![
            GuestOp::Syscall { nr: SOCKET, args: [2, DATAGRAM, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: BIND, args: [FD, ADDR_SELF, 16, 0, 0, 0] },
            GuestOp::Syscall { nr: SOCKET, args: [2, STREAM, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: BIND, args: [FD2, ADDR_SELF, 16, 0, 0, 0] },
            GuestOp::Exit(0),
        ]);
        let preloads = [(0u32, ADDR_SELF, sockaddr_for(0, 9000))];
        let (_, events) = run_networked(vec![both], &preloads, wiring(1), Seed(1));

        let binds: Vec<i64> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Syscall { nr, ret, .. } if *nr == BIND => Some(*ret),
                _ => None,
            })
            .collect();
        assert_eq!(binds, vec![0, 0], "both binds must take the port they asked for");
    }
}

mod disk {
    use bohr_backend_null::{GuestOp, NullProgram, MEMORY_BASE};
    use bohr_testkit::{run_scenario, Scenario};
    use bohr_trace::{DiskOpKind, EventKind, Outcome};
    use bohr_types::{GuestAddr, NodeId, Seed, Tick};
    use bohr_world::{DiskConfig, NodeFaultKind, ScheduledNodeFault};

    const OPENAT: u32 = 257;
    const READ: u32 = 0;
    const WRITE: u32 = 1;
    const PREAD64: u32 = 17;
    const FSYNC: u32 = 74;
    const CLOSE: u32 = 3;
    const FUTEX: u32 = 202;
    const SCHED_YIELD: u32 = 24;
    const O_CREAT: u64 = 0o100;
    /// The first descriptor `openat` hands out; deterministic, so a script can name it.
    const FD: u64 = 3;

    const PATH: u64 = MEMORY_BASE + 900;
    const INBOX: u64 = MEMORY_BASE + 1024;
    const PROBE: u64 = MEMORY_BASE + 2048;
    const TIMEOUT_TS: u64 = MEMORY_BASE + 960;
    const DATA: u64 = MEMORY_BASE + 8192;
    const DATA2: u64 = MEMORY_BASE + 16384;

    fn path_preload(node: u32) -> (u32, u64, Vec<u8>) {
        (node, PATH, b"/db\0".to_vec())
    }

    fn open() -> GuestOp {
        GuestOp::Syscall { nr: OPENAT, args: [0, PATH, O_CREAT, 0, 0, 0] }
    }

    /// A five-millisecond timespec, preloaded where a timed futex wait can point at it.
    fn timeout_preload(node: u32) -> (u32, u64, Vec<u8>) {
        let mut ts = vec![0u8; 16];
        ts[8..16].copy_from_slice(&5_000_000u64.to_le_bytes());
        (node, TIMEOUT_TS, ts)
    }

    fn wait_out_the_run() -> GuestOp {
        GuestOp::Syscall { nr: FUTEX, args: [MEMORY_BASE + 992, 0, 0, TIMEOUT_TS, 0, 0] }
    }

    fn last_read_ret(events: &[bohr_trace::Event]) -> Option<i64> {
        events.iter().rev().find_map(|e| match e.kind {
            EventKind::DiskOp { op: DiskOpKind::Read, ret, .. } => Some(ret),
            _ => None,
        })
    }

    /// TESTING 4.1: what was written and synced is read back after close and reopen.
    #[test]
    fn virtual_disk_roundtrip() {
        let program = NullProgram::single(vec![
            open(),
            GuestOp::Syscall { nr: WRITE, args: [FD, DATA, 5, 0, 0, 0] },
            GuestOp::Syscall { nr: FSYNC, args: [FD, 0, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: CLOSE, args: [FD, 0, 0, 0, 0, 0] },
            open(),
            GuestOp::Syscall { nr: READ, args: [FD, INBOX, 64, 0, 0, 0] },
            GuestOp::Exit(0),
        ]);
        let preloads = [path_preload(0), (0, DATA, b"hello".to_vec())];
        let (report, events) = run_scenario(vec![program], &preloads, Scenario::default(), Seed(1));
        assert_eq!(report.outcome, Outcome::Clean, "got {:?}", report.outcome);
        assert_eq!(last_read_ret(&events), Some(5), "the synced bytes came back");
    }

    /// TESTING 4.2 property, in its sharpest form: nothing survives a crash that was not fsynced.
    #[test]
    fn unsynced_data_is_lost_on_a_crash() {
        let program = NullProgram::single(vec![
            open(),
            GuestOp::Syscall { nr: READ, args: [FD, INBOX, 64, 0, 0, 0] },
            GuestOp::Syscall { nr: WRITE, args: [FD, DATA, 4, 0, 0, 0] },
            wait_out_the_run(),
            GuestOp::Exit(0),
        ]);
        let preloads = [path_preload(0), timeout_preload(0), (0, DATA, b"data".to_vec())];
        let scenario = Scenario {
            node_faults: vec![
                ScheduledNodeFault {
                    at_tick: Tick(200_000),
                    node: NodeId(0),
                    kind: NodeFaultKind::Kill,
                },
                ScheduledNodeFault {
                    at_tick: Tick(1_000_000),
                    node: NodeId(0),
                    kind: NodeFaultKind::Restart,
                },
            ],
            ..Scenario::default()
        };

        let (report, events) = run_scenario(vec![program], &preloads, scenario, Seed(2));
        assert_eq!(report.outcome, Outcome::Clean, "got {:?}", report.outcome);
        let reads: Vec<i64> = events
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::DiskOp { op: DiskOpKind::Read, ret, .. } => Some(ret),
                _ => None,
            })
            .collect();
        assert_eq!(reads.len(), 2, "both incarnations read");
        assert!(reads.iter().all(|r| *r == 0), "an unsynced write survived the crash: {reads:?}");
    }

    /// The control half: the same crash, with an `fsync` before it, loses nothing.
    #[test]
    fn synced_data_survives_the_same_crash() {
        let program = NullProgram::single(vec![
            open(),
            GuestOp::Syscall { nr: READ, args: [FD, INBOX, 64, 0, 0, 0] },
            GuestOp::Syscall { nr: WRITE, args: [FD, DATA, 4, 0, 0, 0] },
            GuestOp::Syscall { nr: FSYNC, args: [FD, 0, 0, 0, 0, 0] },
            wait_out_the_run(),
            GuestOp::Exit(0),
        ]);
        let preloads = [path_preload(0), timeout_preload(0), (0, DATA, b"data".to_vec())];
        let scenario = Scenario {
            node_faults: vec![
                ScheduledNodeFault {
                    at_tick: Tick(200_000),
                    node: NodeId(0),
                    kind: NodeFaultKind::Kill,
                },
                ScheduledNodeFault {
                    at_tick: Tick(1_000_000),
                    node: NodeId(0),
                    kind: NodeFaultKind::Restart,
                },
            ],
            ..Scenario::default()
        };

        let (report, events) = run_scenario(vec![program], &preloads, scenario, Seed(2));
        assert_eq!(report.outcome, Outcome::Clean, "got {:?}", report.outcome);
        assert_eq!(last_read_ret(&events), Some(4), "the synced write outlived the crash");
    }

    /// TESTING 4.2: a kill landing inside a stalled `fsync` persists a sector-aligned prefix of
    /// the appended bytes — a torn write, visible after restart.
    #[test]
    fn a_torn_write_is_visible_after_the_crash() {
        let program = NullProgram::single(vec![
            open(),
            GuestOp::Syscall { nr: READ, args: [FD, INBOX, 8, 0, 0, 0] },
            GuestOp::JumpIfMem { addr: GuestAddr(INBOX), equals: 0, pc: 5 },
            GuestOp::Syscall { nr: PREAD64, args: [FD, PROBE, 4096, 600, 0, 0] },
            GuestOp::Exit(0),
            GuestOp::Syscall { nr: WRITE, args: [FD, DATA, 600, 0, 0, 0] },
            GuestOp::Syscall { nr: FSYNC, args: [FD, 0, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: WRITE, args: [FD, DATA2, 1536, 0, 0, 0] },
            GuestOp::Syscall { nr: FSYNC, args: [FD, 0, 0, 0, 0, 0] },
            GuestOp::Exit(0),
        ]);
        let preloads = [path_preload(0), (0, DATA, vec![0xAB; 600]), (0, DATA2, vec![0xCD; 1536])];
        let scenario = || Scenario {
            disk: DiskConfig {
                // The second fsync must stall past the kill: certain stall, an effectively
                // endless mean.
                stall_probability: (1, 1),
                stall_mean_ticks: 1_000_000_000_000,
                ..DiskConfig::default()
            },
            node_faults: vec![
                ScheduledNodeFault {
                    at_tick: Tick(2_000_000),
                    node: NodeId(0),
                    kind: NodeFaultKind::Kill,
                },
                ScheduledNodeFault {
                    at_tick: Tick(4_000_000),
                    node: NodeId(0),
                    kind: NodeFaultKind::Restart,
                },
            ],
            ..Scenario::default()
        };

        let (report, events) = run_scenario(vec![program.clone()], &preloads, scenario(), Seed(5));
        assert_eq!(report.outcome, Outcome::Clean, "got {:?}", report.outcome);
        let torn = last_read_ret(&events).expect("the probe read");
        assert!(torn % 512 == 0 && torn <= 1536, "the tear is sector-aligned: {torn}");

        for attempt in 0..50 {
            let (again, events_again) =
                run_scenario(vec![program.clone()], &preloads, scenario(), Seed(5));
            assert_eq!(report.digest, again.digest, "attempt {attempt} diverged");
            assert_eq!(last_read_ret(&events_again), Some(torn));
        }
    }

    /// A restart from an older snapshot reopens the disk the earlier `fsync` promised.
    #[test]
    fn a_snapshot_restart_reopens_the_older_image() {
        let spin = 9;
        let program = NullProgram::single(vec![
            open(),
            GuestOp::Syscall { nr: READ, args: [FD, INBOX, 8, 0, 0, 0] },
            GuestOp::JumpIfMem { addr: GuestAddr(INBOX), equals: 0, pc: 5 },
            GuestOp::Syscall { nr: PREAD64, args: [FD, PROBE, 64, 0, 0, 0] },
            GuestOp::Exit(0),
            GuestOp::Syscall { nr: WRITE, args: [FD, DATA, 2, 0, 0, 0] },
            GuestOp::Syscall { nr: FSYNC, args: [FD, 0, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: WRITE, args: [FD, DATA2, 2, 0, 0, 0] },
            GuestOp::Syscall { nr: FSYNC, args: [FD, 0, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: SCHED_YIELD, args: [0; 6] },
            GuestOp::Jump { pc: spin },
        ]);
        let preloads = [path_preload(0), (0, DATA, b"aa".to_vec()), (0, DATA2, b"bb".to_vec())];
        let scenario = Scenario {
            node_faults: vec![
                ScheduledNodeFault {
                    at_tick: Tick(1_000_000),
                    node: NodeId(0),
                    kind: NodeFaultKind::Kill,
                },
                ScheduledNodeFault {
                    at_tick: Tick(2_000_000),
                    node: NodeId(0),
                    kind: NodeFaultKind::RestartFromSnapshot { back: 1 },
                },
            ],
            ..Scenario::default()
        };

        let (report, events) = run_scenario(vec![program], &preloads, scenario, Seed(3));
        assert_eq!(report.outcome, Outcome::Clean, "got {:?}", report.outcome);
        assert_eq!(
            last_read_ret(&events),
            Some(2),
            "the probe sees the first sync's two bytes, not the second's four"
        );
    }

    /// Injected disk errors come off the tape: the same seed fails the same calls, every run.
    #[test]
    fn injected_disk_errors_repeat_exactly() {
        let mut ops = vec![open()];
        for _ in 0..12 {
            ops.push(GuestOp::Syscall { nr: WRITE, args: [FD, DATA, 4, 0, 0, 0] });
            ops.push(GuestOp::Syscall { nr: FSYNC, args: [FD, 0, 0, 0, 0, 0] });
        }
        ops.push(GuestOp::Exit(0));
        let program = NullProgram::single(ops);
        let preloads = [path_preload(0), (0, DATA, b"wxyz".to_vec())];
        let scenario = || Scenario {
            disk: DiskConfig {
                eio_probability: (1, 2),
                enospc_probability: (1, 2),
                ..DiskConfig::default()
            },
            ..Scenario::default()
        };

        let (first, events) = run_scenario(vec![program.clone()], &preloads, scenario(), Seed(7));
        assert_eq!(first.outcome, Outcome::Clean, "got {:?}", first.outcome);
        let rets: Vec<i64> = events
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::DiskOp { ret, .. } => Some(ret),
                _ => None,
            })
            .collect();
        assert!(rets.contains(&-5) || rets.contains(&-28), "no injected failure fired: {rets:?}");

        for attempt in 0..100 {
            let (again, _) = run_scenario(vec![program.clone()], &preloads, scenario(), Seed(7));
            assert_eq!(first.digest, again.digest, "attempt {attempt} diverged");
        }
    }
}

mod cluster_scenarios {
    use bohr_backend_null::{GuestOp, NullProgram, MEMORY_BASE};
    use bohr_testkit::{run_scenario, sockaddr_for, Scenario};
    use bohr_trace::{EventKind, FaultKind, Outcome, PacketFate};
    use bohr_types::{NodeId, Seed, Tick};
    use bohr_world::{NetworkConfig, NodeFaultKind, ScheduledNodeFault, ScheduledPartition};

    const SOCKET: u32 = 41;
    const SENDTO: u32 = 44;
    const RECVFROM: u32 = 45;
    const BIND: u32 = 49;
    const NANOSLEEP: u32 = 35;
    const FD: u64 = 4096;

    const ADDR_SELF: u64 = MEMORY_BASE;
    const ADDR_PEER: u64 = MEMORY_BASE + 16;
    const PAYLOAD: u64 = MEMORY_BASE + 64;
    const INBOX: u64 = MEMORY_BASE + 128;
    const SLEEP_TS: u64 = MEMORY_BASE + 192;

    /// TESTING 4.3 / T6: five nodes in a ring, each passing a datagram to its neighbour and
    /// reading one from the other side, byte-identical across repeats.
    #[test]
    fn five_node_cluster_repeats_exactly() {
        let node = || {
            NullProgram::single(vec![
                GuestOp::Syscall { nr: SOCKET, args: [2, 2, 0, 0, 0, 0] },
                GuestOp::Syscall { nr: BIND, args: [FD, ADDR_SELF, 16, 0, 0, 0] },
                GuestOp::Syscall { nr: SENDTO, args: [FD, PAYLOAD, 4, 0, ADDR_PEER, 16] },
                GuestOp::Syscall { nr: RECVFROM, args: [FD, INBOX, 64, 0, 0, 0] },
                GuestOp::Exit(0),
            ])
        };
        let programs = || (0..5).map(|_| node()).collect::<Vec<_>>();
        let mut preloads = Vec::new();
        for i in 0..5u32 {
            preloads.push((i, ADDR_SELF, sockaddr_for(i, 7000)));
            preloads.push((i, ADDR_PEER, sockaddr_for((i + 1) % 5, 7000)));
            preloads.push((i, PAYLOAD, format!("n{i}ok").into_bytes()));
        }
        let scenario = || Scenario {
            network: NetworkConfig {
                nodes: 5,
                latency_mean_ticks: 200_000,
                latency_jitter_ticks: 100_000,
                ..NetworkConfig::default()
            },
            ..Scenario::default()
        };

        let (first, events) = run_scenario(programs(), &preloads, scenario(), Seed(42));
        assert_eq!(first.outcome, Outcome::Clean, "got {:?}", first.outcome);
        let delivered = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::PacketFate { fate: PacketFate::Delivered, .. }))
            .count();
        assert_eq!(delivered, 5, "every node's datagram went round the ring");

        for attempt in 0..200 {
            let (again, _) = run_scenario(programs(), &preloads, scenario(), Seed(42));
            assert_eq!(first.digest, again.digest, "attempt {attempt} diverged");
        }
    }

    /// C4, the scripted shape: a partition splits the cluster, heals, and the trace carries
    /// exactly the expected fault narration — the run the fault-drift injection must break.
    #[test]
    fn split_brain_then_heal_narrates_exactly() {
        // Node 0 sends into the cut, sleeps past the heal, sends again; node 1 receives the
        // packet that survives.
        let sender = NullProgram::single(vec![
            GuestOp::Syscall { nr: SOCKET, args: [2, 2, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: BIND, args: [FD, ADDR_SELF, 16, 0, 0, 0] },
            GuestOp::Syscall { nr: SENDTO, args: [FD, PAYLOAD, 4, 0, ADDR_PEER, 16] },
            GuestOp::Syscall { nr: NANOSLEEP, args: [SLEEP_TS, 0, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: SENDTO, args: [FD, PAYLOAD, 4, 0, ADDR_PEER, 16] },
            GuestOp::Exit(0),
        ]);
        let receiver = NullProgram::single(vec![
            GuestOp::Syscall { nr: SOCKET, args: [2, 2, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: BIND, args: [FD, ADDR_SELF, 16, 0, 0, 0] },
            GuestOp::Syscall { nr: RECVFROM, args: [FD, INBOX, 64, 0, 0, 0] },
            GuestOp::Exit(0),
        ]);
        let mut sleep_ts = vec![0u8; 16];
        // Sleep 30 ms: past the heal at 20 ms, whatever the schedule does first.
        sleep_ts[8..16].copy_from_slice(&30_000_000u64.to_le_bytes());
        let preloads = [
            (0u32, ADDR_SELF, sockaddr_for(0, 9000)),
            (0, ADDR_PEER, sockaddr_for(1, 7000)),
            (0, PAYLOAD, b"raft".to_vec()),
            (0, SLEEP_TS, sleep_ts),
            (1, ADDR_SELF, sockaddr_for(1, 7000)),
        ];
        let scenario = || Scenario {
            network: NetworkConfig {
                nodes: 2,
                latency_mean_ticks: 500_000,
                partitions: vec![ScheduledPartition {
                    at_tick: Tick(0),
                    heal_at_tick: Tick(20_000_000),
                    cut: vec![(NodeId(0), NodeId(1))],
                }],
                ..NetworkConfig::default()
            },
            ..Scenario::default()
        };

        let (first, events) =
            run_scenario(vec![sender.clone(), receiver.clone()], &preloads, scenario(), Seed(9));
        assert_eq!(first.outcome, Outcome::Clean, "got {:?}", first.outcome);

        let partitions: Vec<u64> = events
            .iter()
            .filter(|e| {
                matches!(e.kind, EventKind::FaultFired { fault: FaultKind::NetPartition, .. })
            })
            .map(|e| e.tick.get())
            .collect();
        assert_eq!(partitions.len(), 2, "one cut raised, one healed");
        assert_eq!(partitions[0], 0, "the cut rises exactly at its scheduled tick");
        assert_eq!(partitions[1], 20_000_000, "the heal lands exactly at its scheduled tick");

        let dropped = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::PacketFate { fate: PacketFate::Dropped, .. }))
            .count();
        let delivered = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::PacketFate { fate: PacketFate::Delivered, .. }))
            .count();
        assert_eq!((dropped, delivered), (1, 1), "the cut ate one send and the heal let one by");

        for attempt in 0..200 {
            let (again, _) = run_scenario(
                vec![sender.clone(), receiver.clone()],
                &preloads,
                scenario(),
                Seed(9),
            );
            assert_eq!(first.digest, again.digest, "attempt {attempt} diverged");
        }
    }

    /// A frozen node misses nothing: the delivery lands while it is frozen and is read after the
    /// thaw — the pause is a pause, not a drop.
    #[test]
    fn a_frozen_node_reads_what_arrived_while_it_slept() {
        let sender = NullProgram::single(vec![
            GuestOp::Syscall { nr: SOCKET, args: [2, 2, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: BIND, args: [FD, ADDR_SELF, 16, 0, 0, 0] },
            GuestOp::Syscall { nr: SENDTO, args: [FD, PAYLOAD, 4, 0, ADDR_PEER, 16] },
            GuestOp::Exit(0),
        ]);
        let receiver = NullProgram::single(vec![
            GuestOp::Syscall { nr: SOCKET, args: [2, 2, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: BIND, args: [FD, ADDR_SELF, 16, 0, 0, 0] },
            GuestOp::Syscall { nr: RECVFROM, args: [FD, INBOX, 64, 0, 0, 0] },
            GuestOp::Exit(0),
        ]);
        let preloads = [
            (0u32, ADDR_SELF, sockaddr_for(0, 9000)),
            (0, ADDR_PEER, sockaddr_for(1, 7000)),
            (0, PAYLOAD, b"late".to_vec()),
            (1, ADDR_SELF, sockaddr_for(1, 7000)),
        ];
        let scenario = Scenario {
            network: NetworkConfig {
                nodes: 2,
                latency_mean_ticks: 1_000_000,
                ..NetworkConfig::default()
            },
            node_faults: vec![
                ScheduledNodeFault {
                    // Past both nodes' startup syscalls, so the receiver is already bound and
                    // parked in its receive when the freeze lands.
                    at_tick: Tick(50_000),
                    node: NodeId(1),
                    kind: NodeFaultKind::Freeze,
                },
                ScheduledNodeFault {
                    at_tick: Tick(10_000_000),
                    node: NodeId(1),
                    kind: NodeFaultKind::Thaw,
                },
            ],
            ..Scenario::default()
        };

        let (report, events) = run_scenario(vec![sender, receiver], &preloads, scenario, Seed(6));
        assert_eq!(report.outcome, Outcome::Clean, "got {:?}", report.outcome);
        assert!(
            report.ticks.get() >= 10_000_000,
            "the receive could only finish after the thaw, reached {:?}",
            report.ticks
        );
        assert!(
            events.iter().any(|e| matches!(
                e.kind,
                EventKind::FaultFired { fault: FaultKind::NodeFreeze, .. }
            )),
            "the freeze narrated itself"
        );
    }

    /// The threaded fixture holds under the cluster world: per-node futex spaces do not disturb
    /// interleaving within one node.
    #[test]
    fn threads_race_deterministically_under_the_cluster_world() {
        use bohr_testkit::{run_cluster, Fixture};
        let program = || vec![Fixture::TwoTasks.program()];
        let (first, _) = run_cluster(program(), &[], Seed(17));
        assert_eq!(first.outcome, Outcome::Clean, "got {:?}", first.outcome);
        for attempt in 0..200 {
            let (again, _) = run_cluster(program(), &[], Seed(17));
            assert_eq!(first.digest, again.digest, "attempt {attempt} diverged");
        }
    }
}

mod sdk {
    use bohr_backend_null::{GuestOp, NullProgram, MEMORY_BASE};
    use bohr_testkit::{run_scenario, Scenario};
    use bohr_trace::{AssertionKind, EventKind};
    use bohr_types::Seed;

    const OPENAT: u32 = 257;
    const WRITE: u32 = 1;
    const O_CREAT: u64 = 0o100;
    /// The first descriptor `openat` hands out, which is deterministic and so can be scripted.
    const FD: u64 = 3;

    const PATH: u64 = MEMORY_BASE;
    const RECORD: u64 = MEMORY_BASE + 256;

    /// The path the world tells a guest to write records to.
    const CHANNEL: &[u8] = b"/bohr/records.jsonl\0";

    /// One record, in the shape an SDK writes.
    const LINE: &str = "{\"antithesis_assert\":{\"hit\":true,\"must_hit\":true,\"assert_type\":\"always\",\"display_type\":\"Always\",\"message\":\"log is monotonic\",\"condition\":false,\"id\":\"log is monotonic\",\"location\":{\"file\":\"src/log.rs\",\"function\":\"append\",\"begin_line\":220,\"begin_column\":9}},\"bohr\":{\"schema\":1,\"seq\":1,\"node\":0}}\n";

    fn guest(len: u64) -> NullProgram {
        NullProgram::single(vec![
            GuestOp::Syscall { nr: OPENAT, args: [0, PATH, O_CREAT, 0, 0, 0] },
            GuestOp::Syscall { nr: WRITE, args: [FD, RECORD, len, 0, 0, 0] },
            GuestOp::Exit(0),
        ])
    }

    fn scenario() -> Scenario {
        Scenario { sdk_output: Some("/bohr/records.jsonl".to_owned()), ..Scenario::default() }
    }

    /// The run names where the promise broke, and it is not where the run stopped.
    ///
    /// A guest carries on after an assertion fails — the write that reported it returns, and the
    /// guest exits on its own terms — so the last event is wherever it happened to finish. A
    /// report that pointed a reader there would point them past the answer.
    #[test]
    fn the_run_names_the_event_the_promise_broke_at() {
        let preloads = [(0u32, PATH, CHANNEL.to_vec()), (0, RECORD, LINE.as_bytes().to_vec())];
        let (report, events) =
            run_scenario(vec![guest(LINE.len() as u64)], &preloads, scenario(), Seed(1));

        let failed = events
            .iter()
            .find(|e| matches!(&e.kind, EventKind::Assertion { condition: false, .. }))
            .expect("the failed assertion is on the timeline");

        assert_eq!(
            report.violation_event,
            Some(failed.id),
            "the run named a different event than the one the assertion failed at"
        );
        assert_ne!(
            Some(failed.id),
            events.last().map(|e| e.id),
            "the assertion happened to be last, so this fixture no longer tests anything"
        );
    }

    /// A guest that writes an assertion gets it on the timeline and in the stored records.
    #[test]
    fn a_written_assertion_reaches_the_trace() {
        let preloads = [(0u32, PATH, CHANNEL.to_vec()), (0, RECORD, LINE.as_bytes().to_vec())];
        let (report, events) =
            run_scenario(vec![guest(LINE.len() as u64)], &preloads, scenario(), Seed(1));

        assert!(
            events.iter().any(|e| matches!(e.kind, EventKind::SdkRecord { .. })),
            "the record never reached the timeline"
        );
        let assertion = events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::Assertion { name, kind, condition, .. } => {
                    Some((name.clone(), *kind, *condition))
                }
                _ => None,
            })
            .expect("the assertion was not recorded as one");
        assert_eq!(assertion.0, "log is monotonic");
        assert_eq!(assertion.1, AssertionKind::Always);
        assert!(!assertion.2, "the condition was recorded as holding when it did not");

        // Recording the violation is the world's half; the verdict is the oracle's, and this
        // harness runs one with no opinion so a determinism test measures reproduction rather
        // than judgement. What must be here is the diagnostic an oracle judges on.
        let violation = events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::Diagnostic { code, .. } if code == "BOHR-2012"));
        assert!(violation, "a failed assertion raised no diagnostic for an oracle to read");
        let _ = &report;
    }

    /// A line that arrives in pieces is one record, not two malformed ones.
    #[test]
    fn a_record_split_across_writes_is_still_one_record() {
        let half = LINE.len() as u64 / 2;
        let program = NullProgram::single(vec![
            GuestOp::Syscall { nr: OPENAT, args: [0, PATH, O_CREAT, 0, 0, 0] },
            GuestOp::Syscall { nr: WRITE, args: [FD, RECORD, half, 0, 0, 0] },
            GuestOp::Syscall {
                nr: WRITE,
                args: [FD, RECORD + half, LINE.len() as u64 - half, 0, 0, 0],
            },
            GuestOp::Exit(0),
        ]);
        let preloads = [(0u32, PATH, CHANNEL.to_vec()), (0, RECORD, LINE.as_bytes().to_vec())];
        let (_, events) = run_scenario(vec![program], &preloads, scenario(), Seed(1));

        let records =
            events.iter().filter(|e| matches!(e.kind, EventKind::SdkRecord { .. })).count();
        assert_eq!(records, 1, "a split line was read as {records} records");
    }

    /// Records are the same whatever else the run does, so a run carrying them still repeats.
    #[test]
    fn a_run_with_records_repeats_exactly() {
        let preloads = [(0u32, PATH, CHANNEL.to_vec()), (0, RECORD, LINE.as_bytes().to_vec())];
        let first =
            run_scenario(vec![guest(LINE.len() as u64)], &preloads, scenario(), Seed(4)).0.digest;
        for attempt in 0..100 {
            let again =
                run_scenario(vec![guest(LINE.len() as u64)], &preloads, scenario(), Seed(4))
                    .0
                    .digest;
            assert_eq!(first, again, "attempt {attempt} diverged");
        }
    }
}
