//! Writes the seeds the fuzz targets start from.
//!
//! Seeds come from the tool's own writers, so they follow format changes.
//!
//! Seeds are few, named and committed; the corpus grown from them stays out of the history. Two are
//! inputs that once crashed a reader.
//!
//! Run it with `make fuzz-seed`.

use std::path::{Path, PathBuf};

use bohr_backend::GuestSpec;
use bohr_backend_null::NullBackend;
use bohr_kernel::{Oracle, RandomPolicy, RunConfig, RunInputs, RunOutcome, Verdict, run};
use bohr_tape::Tape;
use bohr_testkit::Fixture;
use bohr_trace::TraceWriter;
use bohr_types::{OracleId, Seed};

/// Judges nothing; seeds are about formats.
struct AcceptEverything;

impl Oracle for AcceptEverything {
    fn identity(&self) -> OracleId {
        OracleId::new("accept-everything", 1)
    }

    fn judge(&mut self, _outcome: &RunOutcome) -> Verdict {
        Verdict::Clean
    }
}

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let seeds = root.join("fuzz/seeds");

    // A tape as it is stored: the container both `bohr replay` and a mutating fuzzer read.
    write(&seeds, "fuzz_tape", "from-seed-0", &Tape::from_seed(Seed(0)).to_container());
    write(
        &seeds,
        "fuzz_tape",
        "with-prefix",
        &Tape::with_prefix(Seed(1), vec![0xA5; 64]).to_container(),
    );

    // An event log from a real run, with every framing the writer produces.
    for fixture in Fixture::all() {
        write(&seeds, "fuzz_event_log", fixture.name(), &record(fixture));
    }

    // Bundles and SDK output reuse the fixtures' bytes.
    copy_all(&root.join("testdata/bundles"), &seeds.join("fuzz_bundle"), "bohr");
    copy_all(&root.join("testdata/sdk/antithesis"), &seeds.join("fuzz_sdk_record"), "jsonl");

    // Scripted syscalls: a seed, then a number and six registers each, as a real guest opens.
    for (label, script) in [
        ("clock", &[(228u32, [0u64, 0x1000, 0, 0, 0, 0])][..]),
        ("write", &[(1, [1, 0x1000, 16, 0, 0, 0])][..]),
        ("futex", &[(202, [0x2000, 0, 0, 0, 0, 0]), (202, [0x2000, 1, 1, 0, 0, 0])][..]),
    ] {
        write(&seeds, "fuzz_syscall_args", label, &calls(script));
    }

    // The two inputs that once crashed a reader.
    write(&seeds, "fuzz_bundle", "regression-impossible-length", &impossible_length());
    write(
        &seeds,
        "fuzz_syscall_args",
        "regression-unbounded-buffer",
        &calls(&[(318, [0x1000, u64::MAX, 0, 0, 0, 0]), (77, [3, 1 << 50, 0, 0, 0, 0])]),
    );
    write(
        &seeds,
        "fuzz_syscall_args",
        "regression-wrapping-address",
        &calls(&[(7, [u64::MAX - 4, 64, 0, 0, 0, 0])]),
    );

    println!("seeds written under {}", seeds.display());
}

/// A scripted program in the shape the syscall target reads: a seed, then a number and six
/// argument registers per call.
fn calls(calls: &[(u32, [u64; 6])]) -> Vec<u8> {
    let mut bytes = 0u64.to_le_bytes().to_vec();
    for (nr, args) in calls {
        bytes.extend_from_slice(&nr.to_le_bytes());
        for arg in args {
            bytes.extend_from_slice(&arg.to_le_bytes());
        }
    }
    bytes
}

/// A bundle whose section table claims to unpack to more memory than exists.
fn impossible_length() -> Vec<u8> {
    let mut w = bohr_trace::BundleWriter::new();
    w.section(bohr_trace::SectionId::Manifest, vec![b'{'; 4096]);
    let mut bytes = w.finish([0; 16]);
    let at = bohr_trace::BUNDLE_HEADER_BYTES + 24;
    bytes[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    bytes
}

/// Runs one fixture and returns the log it wrote.
fn record(fixture: Fixture) -> Vec<u8> {
    let tape = Tape::from_seed(Seed(0));
    let mut backend = NullBackend::new(fixture.program());
    let mut world = bohr_world::MinimalWorld::new();
    let mut policy = RandomPolicy;
    let mut oracle = AcceptEverything;
    let mut sink = TraceWriter::new(Vec::new());

    run(RunInputs {
        arming: bohr_kernel::Arming::NONE,
        tape: &tape,
        backend: &mut backend,
        world: &mut world,
        policy: &mut policy,
        oracle: &mut oracle,
        sink: &mut sink,
        observer: None,
        specs: std::slice::from_ref(&GuestSpec::default()),
        config: RunConfig::default(),
    })
    .expect("a scripted guest always completes");

    let (log, _, _) = sink.finish().expect("the sink closes");
    log
}

fn write(seeds: &Path, target: &str, name: &str, bytes: &[u8]) {
    let dir = seeds.join(target);
    std::fs::create_dir_all(&dir).expect("the seed directory can be created");
    std::fs::write(dir.join(name), bytes).expect("a seed can be written");
}

/// Copies the fixtures of one kind across, under names of their own.
///
/// The extension is dropped, since the history ignores working copies by extension.
fn copy_all(from: &Path, to: &Path, extension: &str) {
    std::fs::create_dir_all(to).expect("the seed directory can be created");
    let entries = std::fs::read_dir(from).expect("the fixture directory is there");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == extension) {
            let name = path.file_stem().expect("a file has a name");
            std::fs::copy(&path, to.join(name)).expect("a fixture can be copied");
        }
    }
}
