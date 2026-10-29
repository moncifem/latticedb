//! Kani proofs, run by `.github/workflows/kani.yml`; see `docs/verification.md`.
#![allow(clippy::indexing_slicing)]

use super::*;

/// Any valid cursor survives encode then decode unchanged, and the origin does too.
/// Bound: none on the values (every `lsn` and `index` is symbolic); 12 bytes, no loops.
#[kani::proof]
#[kani::unwind(14)]
fn cursor_round_trips() {
    let lsn: u64 = kani::any();
    let index: u32 = kani::any();
    if let Ok(c) = Cursor::new(lsn, index) {
        assert!(lsn != 0);
        assert_eq!(Cursor::from_bytes(&c.to_bytes()), Ok(c));
        assert_eq!(c.lsn(), lsn);
        assert_eq!(c.index(), index);
    } else {
        assert!(lsn == 0);
    }
    assert_eq!(
        Cursor::from_bytes(&Cursor::ORIGIN.to_bytes()),
        Ok(Cursor::ORIGIN)
    );
}

/// Comparing two cursors gives the order of `(lsn, index)`, and comparing their encodings as
/// unsigned byte strings gives the same order. Bound: none on the values; 12 bytes each.
#[kani::proof]
#[kani::unwind(14)]
fn cursor_order_matches_lsn_index_and_bytes() {
    let (l1, i1): (u64, u32) = (kani::any(), kani::any());
    let (l2, i2): (u64, u32) = (kani::any(), kani::any());
    kani::assume(l1 != 0 && l2 != 0);
    let (Ok(a), Ok(b)) = (Cursor::new(l1, i1), Cursor::new(l2, i2)) else {
        return;
    };
    assert_eq!(a.cmp(&b), (l1, i1).cmp(&(l2, i2)));
    assert_eq!(a.to_bytes().cmp(&b.to_bytes()), a.cmp(&b));
}

/// `Cursor::from_bytes` never panics on a slice of up to 14 bytes (two past the encoded
/// length, so both the short and the trailing-bytes paths are covered), and anything it
/// accepts re-encodes to the same bytes.
#[kani::proof]
#[kani::unwind(16)]
fn cursor_decode_never_panics() {
    let raw: [u8; 14] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= raw.len());
    let result = Cursor::from_bytes(&raw[..len]);
    kani::cover!(result.is_ok(), "a cursor decodes");
    if let Ok(c) = result {
        assert_eq!(len, Cursor::ENCODED_LEN);
        assert_eq!(&c.to_bytes()[..], &raw[..len]);
    }
}

/// `ChangeEvent::decode` never panics on a slice of up to 80 bytes. 80 bytes is enough to
/// get past the fixed header (1 + 12 + 4 + 16 + 16 + 16 + 1 turn flag + 4 kind + 2 key
/// length) and into the key and operation tag; it is not enough to reach a maximum-size key
/// or value, so those limits are covered by the unit and property tests, not by this proof.
#[kani::proof]
#[kani::unwind(82)]
fn event_decode_never_panics() {
    let raw: [u8; 80] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= raw.len());
    // The smallest valid event is 73 bytes, so a successful decode is reachable inside the bound.
    let result = ChangeEvent::decode(&raw[..len]);
    kani::cover!(result.is_ok(), "an event decodes");
    kani::cover!(result.is_err(), "an event is refused");
}
