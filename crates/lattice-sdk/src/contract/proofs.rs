//! Kani proofs, run by `.github/workflows/kani.yml`; see `docs/verification.md`.
#![allow(clippy::indexing_slicing)]

use super::*;

/// For any input of up to 6 bytes pushed in two arbitrary pieces: the framer never panics,
/// gives exactly one result per newline, never returns a line containing a newline or longer
/// than the bytes pushed, and never holds more than [`MAX_LINE_BYTES`]. The bound is far
/// below `MAX_LINE_BYTES` (1 MiB), so this proof does not reach the over-long-line branch;
/// that branch is covered by the unit tests.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(10)]
fn framer_bounds_hold() {
    let raw: [u8; 6] = kani::any();
    let cut: usize = kani::any();
    kani::assume(cut <= raw.len());
    let (head, tail) = raw.split_at(cut);
    let mut framer = Framer::new();
    let mut lines = framer.push(head);
    lines.extend(framer.push(tail));
    let newlines = raw.iter().filter(|b| **b == b'\n').count();
    assert_eq!(lines.len(), newlines);
    kani::cover!(lines.iter().any(Result::is_ok), "a line is returned");
    kani::cover!(newlines == 0, "no newline, bytes stay buffered");
    for bytes in lines.iter().flatten() {
        assert!(bytes.len() <= raw.len());
        assert!(bytes.len() <= MAX_LINE_BYTES);
        assert!(!bytes.contains(&b'\n'));
    }
    assert!(framer.buf.len() <= MAX_LINE_BYTES);
    assert!(framer.buf.len() <= raw.len());
}

/// `finish` after any input of up to 6 bytes never panics and leaves the framer empty.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(10)]
fn framer_finish_resets() {
    let raw: [u8; 6] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= raw.len());
    let mut framer = Framer::new();
    let _ = framer.push(&raw[..len]);
    let _ = framer.finish();
    assert!(framer.buf.is_empty());
    assert!(!framer.skipping);
}
