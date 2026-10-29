//! Exhaustive crash-point sweep: run one fixed workload, count its mutating storage operations,
//! then replay it once per operation with the power cut exactly there. After every cut the machine
//! restarts, recovery runs, and the invariants and the acknowledged set are checked. This is the
//! "crash at every fsync boundary" test, not a random sample of crash points.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use lattice_core::changeset::{ChangeSet, ChangeSetId, TurnRef};
use lattice_core::{Config, Kernel, Receipt};
use lattice_seams::Rng;
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::invariants::{Expected, LogView, Oracle, check_all};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use std::rc::Rc;

type K = Kernel<SharedIo, SharedClock, SeededRng>;

fn cs(
    k: &mut K,
    rng: &mut SeededRng,
    turn: Option<TurnRef>,
    parents: &[ChangeSetId],
) -> (ChangeSet, Expected) {
    let id = k.new_id();
    let payload: Vec<u8> = (0..rng.below(40))
        .map(|_| (rng.next_u64() & 0xFF) as u8)
        .collect();
    let mut c = ChangeSet::new(id, payload.clone());
    c.author = u128::from(rng.next_u64());
    c.turn = turn;
    c.parents = parents.to_vec();
    let e = Expected {
        payload,
        author: c.author,
        turn,
        parents: parents.to_vec(),
    };
    (c, e)
}

/// The fixed workload. Returns what was acknowledged. Stops at the first failure (a power cut).
fn workload(k: &mut K, rng: &mut SeededRng, oracle: &mut Oracle) {
    let mut last: Vec<ChangeSetId> = Vec::new();
    macro_rules! commit {
        ($turn:expr) => {{
            let (c, e) = cs(k, rng, $turn, &last);
            let id = c.id;
            match k.commit(c) {
                Ok(Receipt::Recorded { .. }) => {
                    oracle.acked.insert(id, e);
                    last = vec![id];
                }
                _ => return,
            }
        }};
    }
    if k.begin_turn(TurnRef {
        session: 1,
        turn: 1,
    })
    .is_err()
    {
        return;
    }
    for _ in 0..3 {
        commit!(None);
    }
    let batch: Vec<(ChangeSet, Expected)> = (0..3).map(|_| cs(k, rng, None, &last)).collect();
    let ids: Vec<ChangeSetId> = batch.iter().map(|(c, _)| c.id).collect();
    let results = k.commit_batch(batch.iter().map(|(c, _)| c.clone()).collect());
    for ((id, (_, e)), r) in ids.iter().zip(batch.iter()).zip(results.iter()) {
        match r {
            Ok(Receipt::Recorded { .. }) => {
                oracle.acked.insert(*id, e.clone());
            }
            _ => return,
        }
    }
    if k.begin_turn(TurnRef {
        session: 1,
        turn: 2,
    })
    .is_err()
    {
        return;
    }
    for _ in 0..8 {
        commit!(Some(TurnRef {
            session: 1,
            turn: 2
        }));
    }
}

fn open(disk: &SharedIo, clock: &Rc<SimClock>, seed: u64, segment_limit: u64) -> Result<K, String> {
    let config = Config {
        dir: "wal".into(),
        segment_limit,
        node: 1,
    };
    Kernel::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(seed),
        &config,
    )
    .map(|(k, _)| k)
    .map_err(|e| e.to_string())
}

/// Runs the workload with the cut at operation `cut` (or none), then restarts and verifies.
fn run_cut(seed: u64, cut: Option<u64>, segment_limit: u64) -> (u64, usize) {
    let disk = SharedIo::new(SimIo::new(seed, FaultProfile::crash_heavy()));
    let clock = Rc::new(SimClock::new(1_000));
    let mut oracle = Oracle::default();
    let mut rng = SeededRng::new(seed ^ 77);
    if let Ok(mut k) = open(&disk, &clock, seed, segment_limit) {
        if let Some(n) = cut {
            // Count from here: opening a fresh log already cost some operations.
            disk.arm_power_cut(n);
        }
        workload(&mut k, &mut rng, &mut oracle);
        drop(k);
    }
    let ops = disk.mutating_ops();
    // Restart: power loss, then recovery on a healthy machine.
    disk.crash();
    disk.set_profile(FaultProfile::crash_heavy().without_transient_faults());
    let k = open(&disk, &clock, seed + 1, segment_limit).unwrap_or_else(|e| {
        panic!("seed {seed} cut {cut:?}: recovery failed after a power cut: {e}")
    });
    // Re-open once more and capture the same recovery trace for the unchanged oracle.
    drop(k);
    let config = Config {
        dir: "wal".into(),
        segment_limit,
        node: 1,
    };
    let mut records = Vec::new();
    let (mut k2, _, _) = Kernel::open_streaming(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(seed + 2),
        &config,
        |record| records.push(record),
    )
    .unwrap();
    let view = LogView::from_records(&records);
    let found = check_all(&view, &oracle);
    assert!(found.is_empty(), "seed {seed} cut {cut:?}: {found:?}");
    // The kernel must be usable after recovery: dedupe survived and new commits work.
    for (id, e) in oracle.acked.iter().take(3) {
        let mut again = ChangeSet::new(*id, e.payload.clone());
        again.author = e.author;
        again.turn = e.turn;
        again.parents = e.parents.clone();
        assert_eq!(
            k2.commit(again),
            Ok(Receipt::Duplicate),
            "seed {seed} cut {cut:?}: dedupe lost"
        );
    }
    let id = k2.new_id();
    assert!(matches!(
        k2.commit(ChangeSet::new(id, vec![1])),
        Ok(Receipt::Recorded { .. })
    ));
    (ops, oracle.acked.len())
}

/// Cuts the power at every storage operation of the workload, for each of `seeds` workloads.
/// Returns (cut points swept, cut points after at least one acknowledged commit).
fn sweep(seeds: u64) -> (usize, usize) {
    let mut scenarios = 0;
    let mut cut_with_acks = 0;
    for seed in 0..seeds {
        // A small segment limit forces several rotations into the workload.
        let (total_ops, _) = run_cut(seed, None, 300);
        assert!(
            total_ops > 20,
            "workload should perform many storage operations, got {total_ops}"
        );
        for cut in 0..=total_ops + 1 {
            let (_, acked) = run_cut(seed, Some(cut), 300);
            scenarios += 1;
            cut_with_acks += usize::from(acked > 0);
        }
    }
    (scenarios, cut_with_acks)
}

#[test]
fn inv_i1_every_single_power_cut_point_of_the_workload_is_survivable() {
    let (scenarios, cut_with_acks) = sweep(6);
    eprintln!("swept {scenarios} power-cut points, {cut_with_acks} after acknowledged commits");
    assert!(
        scenarios >= 100,
        "the sweep must cover at least 100 crash points, covered {scenarios}"
    );
    assert!(
        cut_with_acks > scenarios / 2,
        "most cut points should come after some acknowledged commits"
    );
}

/// The gate's volume: 100 distinct workloads, each cut at every one of its storage operations.
/// Runs nightly (`cargo test -p lattice-testkit --release --test power_cut_sweep -- --ignored`).
#[test]
#[ignore = "volume: 100 workloads swept exhaustively; run nightly with --ignored"]
fn inv_i1_hundred_workloads_swept_at_every_power_cut_point() {
    let (scenarios, cut_with_acks) = sweep(100);
    eprintln!(
        "swept {scenarios} power-cut points across 100 workloads, {cut_with_acks} after acknowledged commits"
    );
    assert!(cut_with_acks > scenarios / 2);
}
