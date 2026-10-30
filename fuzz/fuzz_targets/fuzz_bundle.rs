//! Arbitrary bytes offered to the bundle reader.
//!
//! A bundle arrives from somewhere else — a colleague, a CI artefact, an issue attachment — so
//! the reader must refuse anything malformed rather than panic, allocate without bound, or loop.
//! Every refusal is an error with a name; what is being fuzzed is that there is no other outcome.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(bundle) = bohr_trace::Bundle::parse(data) {
        // A bundle that parsed must survive being read: every section, and the manifest if it
        // has one. A parse that succeeded and a read that panics is the same defect, later.
        for id in bundle.section_ids() {
            let _ = bundle.raw_section(id);
        }
        let _ = bundle.manifest();
        let _ = bundle.to_writer();
    }
});
