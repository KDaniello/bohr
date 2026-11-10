//! Arbitrary bytes offered to the bundle reader.
//!
//! Bundles come from elsewhere, so the reader refuses malformed input with a named error and never
//! panics, allocates without bound or loops.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(bundle) = bohr_trace::Bundle::parse(data) {
        // A parsed bundle reads every section and its manifest without panicking.
        for id in bundle.section_ids() {
            let _ = bundle.raw_section(id);
        }
        let _ = bundle.manifest();
        let _ = bundle.to_writer();
    }
});
