//! Pins the non-prefix power-loss shapes of the simulated disk (issue 442): out-of-order page
//! persistence and zero-filled tails. The expectations are derived from the shape's definition, not
//! from the code: the pending bytes below are never zero, so any zero in what survived is a lost
//! page or a zero fill, and a result is a prefix of the pending bytes exactly when it has no zero.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_seams::Io;
use lattice_sim::{FaultProfile, SIM_PAGE_BYTES, SimIo};

/// Synced bytes that precede the unsynced ones. Not a multiple of the page size, so the first
/// unsynced page is shared with durable data.
const DURABLE: usize = 40;
const PENDING: usize = 150;

fn durable_bytes() -> Vec<u8> {
    (0..DURABLE).map(|i| (i % 250) as u8 + 1).collect()
}

/// Never zero, and distinct from the durable bytes at the same position.
fn pending_bytes() -> Vec<u8> {
    (0..PENDING)
        .map(|i| ((i * 7 + 3) % 250) as u8 + 1)
        .collect()
}

fn crashed(seed: u64, profile: FaultProfile) -> (Vec<u8>, u64) {
    let mut io = SimIo::new(seed, profile);
    io.create_dir_all("d").unwrap();
    io.append("d/f", &durable_bytes()).unwrap();
    io.sync("d/f").unwrap();
    io.sync_dir("d").unwrap();
    io.sync_dir("").unwrap();
    io.append("d/f", &pending_bytes()).unwrap();
    io.crash();
    (io.durable_content("d/f").unwrap(), io.shape_events())
}

fn shapes_only() -> FaultProfile {
    FaultProfile {
        garbage_tail: false,
        lose_unsynced_dirs: false,
        ..FaultProfile::non_prefix()
    }
}

#[test]
fn a_crash_never_touches_synced_bytes_and_never_invents_data() {
    for seed in 0..400 {
        let (content, _) = crashed(seed, shapes_only());
        assert_eq!(content[..DURABLE], durable_bytes(), "seed {seed}");
        let kept = &content[DURABLE..];
        assert!(kept.len() <= PENDING, "seed {seed}: grew by {}", kept.len());
        for (i, (got, want)) in kept.iter().zip(pending_bytes()).enumerate() {
            assert!(
                *got == want || *got == 0,
                "seed {seed}: byte {i} is {got}, neither the written byte {want} nor zero"
            );
        }
    }
}

#[test]
fn a_result_with_a_zero_is_exactly_a_counted_non_prefix_event() {
    let pending = pending_bytes();
    let (mut prefixes, mut zero_tails, mut gaps) = (0, 0, 0);
    for seed in 0..400 {
        let (content, events) = crashed(seed, shapes_only());
        let kept = &content[DURABLE..];
        let has_zero = kept.contains(&0);
        assert_eq!(
            events,
            u64::from(has_zero),
            "seed {seed}: shape_events must count exactly the results that are not a prefix"
        );
        if !has_zero {
            assert_eq!(kept, &pending[..kept.len()], "seed {seed}");
            prefixes += 1;
        } else if let Some(first_zero) = kept.iter().position(|b| *b == 0) {
            let zeros_to_end = kept[first_zero..].iter().all(|b| *b == 0);
            if zeros_to_end {
                zero_tails += 1;
            } else {
                gaps += 1;
            }
        }
    }
    // Each of the three shapes is reachable, so the checks above are not vacuous.
    assert!(prefixes > 20, "prefix tears: {prefixes}");
    assert!(zero_tails > 20, "zero-filled tails: {zero_tails}");
    assert!(gaps > 20, "a persisted byte behind a zero: {gaps}");
}

#[test]
fn out_of_order_persistence_works_on_whole_pages_of_the_absolute_file_grid() {
    let only_reorder = FaultProfile {
        torn_writes: false,
        zero_fill_tails: false,
        ..shapes_only()
    };
    let pending = pending_bytes();
    let mut gap_seen = false;
    for seed in 0..400 {
        let (content, events) = crashed(seed, only_reorder);
        let kept = &content[DURABLE..];
        // Within one page of the absolute grid, either every unsynced byte survived or none did.
        let mut page_state = std::collections::BTreeMap::new();
        for (i, got) in kept.iter().enumerate() {
            let page = (DURABLE + i) / SIM_PAGE_BYTES;
            let survived = *got == pending[i];
            let entry = page_state.entry(page).or_insert(survived);
            assert_eq!(
                *entry, survived,
                "seed {seed}: page {page} is partly persisted"
            );
        }
        // The file ends where a persisted page ends, or where the unsynced bytes end.
        let end = DURABLE + kept.len();
        assert!(
            kept.is_empty() || end.is_multiple_of(SIM_PAGE_BYTES) || end == DURABLE + PENDING,
            "seed {seed}: file ends mid-page at {end}"
        );
        // A gap is a lost page followed by a persisted one; the file never ends on a lost page.
        if let Some(last) = kept.last() {
            assert_ne!(
                *last, 0,
                "seed {seed}: the last page must be a persisted one"
            );
        }
        gap_seen |= events == 1;
    }
    assert!(
        gap_seen,
        "no seed lost an earlier page behind a persisted later page"
    );
}

#[test]
fn zero_filled_tails_keep_a_prefix_then_zeros_and_never_a_persisted_byte_after_a_zero() {
    let only_zero_fill = FaultProfile {
        torn_writes: false,
        reorder_pages: false,
        ..shapes_only()
    };
    let pending = pending_bytes();
    for seed in 0..400 {
        let (content, _) = crashed(seed, only_zero_fill);
        let kept = &content[DURABLE..];
        let keep = kept.iter().position(|b| *b == 0).unwrap_or(kept.len());
        assert_eq!(kept[..keep], pending[..keep], "seed {seed}");
        assert!(kept[keep..].iter().all(|b| *b == 0), "seed {seed}");
    }
}

#[test]
fn profiles_without_the_new_flags_keep_prefix_semantics_and_report_no_events() {
    let plain = FaultProfile {
        garbage_tail: false,
        lose_unsynced_dirs: false,
        ..FaultProfile::crash_heavy()
    };
    let pending = pending_bytes();
    for seed in 0..200 {
        let (content, events) = crashed(seed, plain);
        let kept = &content[DURABLE..];
        assert_eq!(kept, &pending[..kept.len()], "seed {seed}");
        assert_eq!(events, 0, "seed {seed}");
    }
}

#[test]
fn the_new_profile_is_what_its_name_says() {
    let p = FaultProfile::non_prefix();
    assert!(p.torn_writes && p.garbage_tail && p.lose_unsynced_dirs);
    assert!(p.reorder_pages && p.zero_fill_tails);
    assert_eq!(
        (p.sync_error_ppm, p.append_error_ppm, p.bit_rot_ppm),
        (0, 0, 0)
    );
    assert_eq!(p.disk_full_after, None);
    let t = p.without_transient_faults();
    assert!(t.reorder_pages && t.zero_fill_tails);
    for old in [
        FaultProfile::clean(),
        FaultProfile::crash_heavy(),
        FaultProfile::disk_faulty(),
        FaultProfile::everything(),
    ] {
        assert!(!old.reorder_pages && !old.zero_fill_tails);
    }
}

#[test]
fn the_same_seed_gives_the_same_shape() {
    for seed in 0..50 {
        assert_eq!(
            crashed(seed, FaultProfile::non_prefix()),
            crashed(seed, FaultProfile::non_prefix())
        );
    }
}
