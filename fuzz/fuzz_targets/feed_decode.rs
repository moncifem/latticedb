#![no_main]
use lattice_core::feed::{ChangeEvent, Cursor};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Only canonical bytes decode, so whatever decodes must re-encode to the exact input.
    if let Ok(ev) = ChangeEvent::decode(data) {
        assert_eq!(ev.encode().as_deref(), Ok(data));
    }
    if let Ok(c) = Cursor::from_bytes(data) {
        assert_eq!(c.to_bytes().as_slice(), data);
    }
});
