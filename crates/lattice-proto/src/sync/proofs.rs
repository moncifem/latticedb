//! Kani proofs, run by `.github/workflows/kani.yml`; see `docs/verification.md`.
#![allow(clippy::indexing_slicing)]

use super::*;

/// `decode` never panics on a byte string of up to 7 bytes (the five header bytes plus a
/// payload of up to 2), and when it returns a frame the length it reports is within the
/// input. A bounded proof: larger payloads, and so the largest events and texts, are
/// covered by the property and fuzz tests only.
#[kani::proof]
#[kani::unwind(9)]
fn frame_decode_never_panics() {
    let raw: [u8; 7] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= raw.len());
    let result = decode(&raw[..len]);
    // Not vacuous. No complete frame fits in 7 bytes (the smallest, a ping, needs 13), so the
    // success arm below is only a guard; the proof covers the header and refusal paths.
    kani::cover!(
        matches!(result, Ok(None)),
        "an incomplete frame waits for more"
    );
    kani::cover!(result.is_err(), "a frame is refused");
    if let Ok(Some((_, used))) = result {
        assert!(used >= HEADER_LEN && used <= len);
    }
}

/// The same with a symbolic payload limit, including limits above and below the frame.
#[kani::proof]
#[kani::unwind(9)]
fn frame_decode_with_limit_never_panics() {
    let raw: [u8; 7] = kani::any();
    let len: usize = kani::any();
    let limit: usize = kani::any();
    kani::assume(len <= raw.len());
    let result = decode_with_limit(&raw[..len], limit);
    kani::cover!(result.is_err(), "a frame is refused under the limit");
    if let Ok(Some((_, used))) = result {
        assert!(used >= HEADER_LEN && used <= len);
        assert!(used - HEADER_LEN <= limit.min(MAX_PAYLOAD));
    }
}
