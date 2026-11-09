//! Whether the search still reaches the defects we planted for it.
//!
//! Correctness gates say we did not break anything. This one says the tool is still *good*, which
//! is a different question and the only one that measures the product's actual claim: that it
//! reaches what ordinary testing cannot.
//!
//! **Reach, not rate.** A defect of depth 3 needs three things to fall together, and reaching one
//! at all is the qualitative difference. How quickly is a property of the machine and of the fault
//! probability — and raising that probability is the cheapest way to make any defect appear
//! sooner, which is more chaos rather than a better search. So the assertion is that the campaign
//! finds it, and the time it took is printed rather than asserted on.
//!
//! Slow by construction: each run is a three-node cluster under interception. Run nightly, and
//! skipped by name where the example has not been built.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::{path::PathBuf, process::Command, time::Instant};

/// Runs in one campaign, which is the budget `docs/TESTING.md` §8 records for this defect: about
/// ten minutes on the machine the corpus was measured on.
///
/// Not a threshold — the budget bounds the experiment and the reach is what is asserted. Chosen
/// from the documented figure rather than from what happens to pass: a budget picked to make the
/// number look right is the same mistake as a fault probability picked that way.
const RUNS: &str = "256";

/// Where the repository sits, from this crate.
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The raft-toy guest, if `make example-raft-toy` has been run.
fn guest() -> Option<PathBuf> {
    let path = root().join("examples/raft-toy/build/raft-toy-node");
    path.is_file().then_some(path)
}

/// Runs one campaign and says whether it reached a finding.
fn campaign(config: &str, master: u32) -> bool {
    let guest = guest().expect("checked by the caller");
    let dir = std::env::temp_dir().join(format!("bohr-hunt-{master}-{}", config.len()));
    let _ = std::fs::remove_dir_all(&dir);

    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .args([
            "explore",
            "--config",
            &root().join(config).to_string_lossy(),
            "--runs",
            RUNS,
            "--master-seed",
            &master.to_string(),
            "--findings",
            &dir.join("findings").to_string_lossy(),
            "-j",
            "4",
            "--",
            &guest.to_string_lossy(),
        ])
        .output()
        .expect("the binary under test must be runnable");

    match out.status.code() {
        // One is a finding, which is what a campaign against a seeded defect is looking for.
        Some(1) => true,
        Some(0) => false,
        other => panic!(
            "the campaign did not complete ({other:?}): {}",
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

/// The milestone gate: the search reaches a depth-3 consensus defect, and reaches it as a matter of
/// course rather than on a lucky seed.
// The host clock is banned everywhere a run could read it, and this is not one of those places: it
// is read once, outside every campaign, and what it produces is printed rather than asserted on.
#[allow(clippy::disallowed_methods)]
#[test]
#[ignore = "nightly: each run is a traced three-node cluster"]
fn the_search_reaches_a_depth_three_defect() {
    if guest().is_none() {
        eprintln!("examples/raft-toy is not built; run `make example-raft-toy`");
        return;
    }

    let started = Instant::now();
    let masters: Vec<u32> = (1..=8).collect();
    let total = masters.len();
    let reached =
        masters.iter().filter(|m| campaign("examples/raft-toy/scenario.toml", **m)).count();

    // Printed rather than asserted on. A search that has become twice as slow is worth noticing;
    // it is not what this milestone claims.
    eprintln!("reached on {reached} of {total} master seeds in {:?}", started.elapsed());

    assert!(
        reached * 2 > total,
        "the search reached the defect on {reached} of {total} master seeds, which is luck rather \
         than reach"
    );
}

/// The other half, and the one that keeps the gate meaningful: without faults the defect is not
/// there to be found. A defect an ordinary interleaving reaches would make the gate measure the
/// scheduler instead of the search.
#[test]
#[ignore = "nightly: each run is a traced three-node cluster"]
fn the_defect_is_out_of_reach_without_faults() {
    if guest().is_none() {
        eprintln!("examples/raft-toy is not built; run `make example-raft-toy`");
        return;
    }
    assert!(
        !campaign("examples/raft-toy/scenario-no-faults.toml", 1),
        "the defect was reached with no faults injected; it is not the depth-3 defect it claims \
         to be, and the gate would be measuring the scheduler"
    );
}

/// The chain, if `make example-chain-toy probe` has been run.
fn chain() -> Option<PathBuf> {
    let path = root().join("examples/chain-toy/build/chain-toy-node");
    path.is_file().then_some(path)
}

/// Runs the chain once under a scenario and says which probe codes the run reported.
fn chain_codes(scenario: &str, seed: u32) -> Vec<String> {
    let node = chain().expect("checked by the caller");
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .current_dir(root())
        .args(["run", "--seed", &seed.to_string(), "--config", scenario, "--"])
        .arg(&node)
        .output()
        .expect("the binary under test must be runnable");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        matches!(out.status.code(), Some(0 | 1)),
        "the run did not complete ({:?}): {said}",
        out.status.code()
    );
    ["BOHR-2040", "BOHR-2041", "BOHR-2042", "BOHR-2043"]
        .into_iter()
        .filter(|code| said.contains(code))
        .map(str::to_owned)
        .collect()
}

/// Each blockchain oracle reports its own code on most seeds of the chain with its defect.
///
/// Ten seeds rather than the sixty the gate in `docs/TESTING.md` §8 uses: this is the nightly check
/// that the reach has not collapsed, and the gate is where the number is recorded.
#[test]
#[ignore = "nightly: each run is a traced three-node chain and its probe"]
fn each_chain_oracle_catches_its_defect() {
    if chain().is_none() {
        eprintln!("examples/chain-toy is not built; run `make example-chain-toy probe`");
        return;
    }
    for (flag, code) in [
        ("revert-finality", "BOHR-2040"),
        ("equivocate", "BOHR-2043"),
        ("fork-on-partition", "BOHR-2041"),
        ("stall-after-heal", "BOHR-2042"),
    ] {
        let scenario = format!("examples/chain-toy/scenario-{flag}.toml");
        let caught =
            (1..=10).filter(|seed| chain_codes(&scenario, *seed).iter().any(|c| c == code)).count();
        assert!(caught > 5, "--{flag}: {code} on {caught} of 10 seeds");
    }
}

/// And none of them reports anything on the chain without a defect.
#[test]
#[ignore = "nightly: each run is a traced three-node chain and its probe"]
fn no_chain_oracle_cries_wolf() {
    if chain().is_none() {
        eprintln!("examples/chain-toy is not built; run `make example-chain-toy probe`");
        return;
    }
    for seed in 1..=10 {
        let codes = chain_codes("examples/chain-toy/scenario.toml", seed);
        assert!(codes.is_empty(), "seed {seed}: {codes:?} on the correct chain");
    }
}
