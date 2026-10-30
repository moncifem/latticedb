//! Kani proofs, run by `.github/workflows/kani.yml`; see `docs/verification.md`.
#![allow(clippy::indexing_slicing)]

use super::*;

/// `decode` never panics on a byte string of up to 14 bytes (the five header bytes plus a
/// payload of up to 9), and when it returns a frame the length it reports is within the
/// input. A bounded proof: larger payloads, and so the largest events and texts, are
/// covered by the property and fuzz tests only.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(16)]
fn frame_decode_never_panics() {
    let raw: [u8; 14] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= raw.len());
    let result = decode(&raw[..len]);
    // Not vacuous: a complete frame (a 13 byte ping) is reachable inside the bound.
    kani::cover!(matches!(result, Ok(Some(_))), "a frame decodes");
    kani::cover!(result.is_err(), "a frame is refused");
    if let Ok(Some((_, used))) = result {
        assert!(used >= HEADER_LEN && used <= len);
    }
}

/// The same with a symbolic payload limit, including limits above and below the frame.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(16)]
fn frame_decode_with_limit_never_panics() {
    let raw: [u8; 14] = kani::any();
    let len: usize = kani::any();
    let limit: usize = kani::any();
    kani::assume(len <= raw.len());
    let result = decode_with_limit(&raw[..len], limit);
    kani::cover!(
        matches!(result, Ok(Some(_))),
        "a frame decodes under the limit"
    );
    if let Ok(Some((_, used))) = result {
        assert!(used >= HEADER_LEN && used <= len);
        assert!(used - HEADER_LEN <= limit.min(MAX_PAYLOAD));
    }
}
