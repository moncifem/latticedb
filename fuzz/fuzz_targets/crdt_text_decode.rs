#![no_main]
use lattice_core::crdt_text::Op;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Only canonical bytes decode, so whatever decodes must re-encode to the exact input.
    if let Ok(op) = Op::decode(data) {
        assert_eq!(op.encode().as_deref(), Ok(data));
    }
});
