//! Cancel registry edges found by mutation testing: the reported capacity and emptiness, and keys
//! that differ from a live one only in the top bit of a word, which must never match.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_api::cancel::{CancelKey, CancelRegistry};
use lattice_seams::Rng;

struct Fixed(u64);
impl Rng for Fixed {
    fn next_u64(&mut self) -> u64 {
        self.0
    }
}

#[test]
fn capacity_and_emptiness_are_reported() {
    let mut registry = CancelRegistry::new(5).unwrap();
    assert_eq!(registry.capacity(), 5);
    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);
    let key = registry.register(&mut Fixed(7), 0, 100).unwrap();
    assert!(!registry.is_empty());
    assert_eq!(registry.len(), 1);
    assert!(registry.unregister(key, 1));
    assert!(registry.is_empty());
    assert_eq!(CancelRegistry::new(1).unwrap().capacity(), 1);
}

#[test]
fn a_key_differing_only_in_the_top_bit_of_either_word_does_not_match() {
    let mut registry = CancelRegistry::new(3).unwrap();
    let key = registry.register(&mut Fixed(0x1234_5678), 0, 100).unwrap();
    let flipped_secret = CancelKey {
        pid: key.pid,
        secret: key.secret ^ i32::MIN,
    };
    let flipped_pid = CancelKey {
        pid: key.pid ^ i32::MIN,
        secret: key.secret,
    };
    let flipped_both = CancelKey {
        pid: key.pid ^ i32::MIN,
        secret: key.secret ^ i32::MIN,
    };
    for wrong in [flipped_secret, flipped_pid, flipped_both] {
        assert!(!registry.cancel(wrong, 1), "{wrong:?}");
        assert!(registry.begin_query(wrong, 1).is_err(), "{wrong:?}");
        assert!(!registry.unregister(wrong, 1), "{wrong:?}");
    }
    // The real key still works afterwards.
    assert!(registry.cancel(key, 2));
}

#[test]
fn cancel_errors_print_what_went_wrong() {
    use lattice_api::cancel::CancelError;
    for (error, text) in [
        (
            CancelError::InvalidCapacity,
            "invalid cancel registry capacity",
        ),
        (CancelError::InvalidLifetime, "invalid cancel key lifetime"),
        (CancelError::ExpiryOverflow, "cancel key expiry overflow"),
        (CancelError::Full, "cancel registry full"),
        (
            CancelError::PidExhausted,
            "cancel registry pid range exhausted",
        ),
        (CancelError::UnknownKey, "unknown cancel key"),
    ] {
        assert_eq!(error.to_string(), text);
    }
}
