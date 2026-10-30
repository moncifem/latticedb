#![no_main]
use lattice_core::crdt_tree::Op;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Only the exact 41-byte layout decodes, so whatever decodes must re-encode to the input.
    if let Ok(op) = Op::decode(data) {
        assert_eq!(op.encode().as_slice(), data);
    }
});
