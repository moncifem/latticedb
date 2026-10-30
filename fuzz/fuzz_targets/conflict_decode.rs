#![no_main]
use lattice_core::workflow::ConflictEntity;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Only bytes that `encode` could have produced decode, so a conflict that decodes re-encodes
    // to the exact input and decodes to an equal value.
    if let Ok(conflict) = ConflictEntity::decode(data) {
        assert_eq!(conflict.encode().as_deref(), Ok(data));
        assert_eq!(ConflictEntity::decode(data).as_ref(), Ok(&conflict));
    }
});
