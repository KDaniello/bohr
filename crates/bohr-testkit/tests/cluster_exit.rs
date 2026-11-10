//! A node that failed is a finding whoever exits after it.
//!
//! A cluster's caller often outlives the node it drove and exits 0. The run keeps the node's
//! failure as its status. Against a real execution client, two seeds in twenty left the client
//! unable to start.

use bohr_backend_null::{GuestOp, NullProgram};
use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{Scenario, judge_scenario};
use bohr_trace::Outcome;
use bohr_types::Seed;
use bohr_world::NetworkConfig;

/// `sched_yield`, which is how a scripted guest lets another one go first.
const SCHED_YIELD: u32 = 24;

/// Ends at once, badly.
fn fails_at_once(code: i32) -> NullProgram {
    NullProgram::single(vec![GuestOp::Exit(code)])
}

/// Yields for a while, then ends well, so it exits last.
fn succeeds_later() -> NullProgram {
    let mut ops: Vec<GuestOp> =
        (0..8).map(|_| GuestOp::Syscall { nr: SCHED_YIELD, args: [0; 6] }).collect();
    ops.push(GuestOp::Exit(0));
    NullProgram::single(ops)
}

fn judge(programs: Vec<NullProgram>) -> Outcome {
    let scenario = Scenario {
        network: NetworkConfig { nodes: 2, ..NetworkConfig::default() },
        ..Scenario::default()
    };
    judge_scenario(programs, &[], scenario, &Tape::from_seed(Seed(1)), RunConfig::default())
        .report
        .outcome
}

/// The first failure is the run's status.
#[test]
fn a_node_that_failed_is_not_hidden_by_a_caller_that_finished() {
    match judge(vec![fails_at_once(1), succeeds_later()]) {
        Outcome::GuestCrashed(why) => {
            assert!(why.contains('1'), "the verdict must name the status: {why}");
        }
        other => panic!("a cluster whose node exited 1 was judged {other:?}"),
    }
}

/// A cluster where nothing failed is clean.
#[test]
fn a_cluster_where_every_node_succeeded_is_clean() {
    assert_eq!(judge(vec![succeeds_later(), succeeds_later()]), Outcome::Clean);
}
