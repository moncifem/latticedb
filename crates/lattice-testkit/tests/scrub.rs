//! The scrubber is an independent read-only verifier. It must find each kind of damage and each
//! violated invariant, and it must stay quiet on a healthy log.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use lattice_core::bugs::{self, Bug};
use lattice_core::changeset::{ChangeSet, TurnRef};
use lattice_core::scrub::{Issue, scrub};
use lattice_core::{Config, Kernel};
use lattice_seams::Io;
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use std::rc::Rc;

type K = Kernel<SharedIo, SharedClock, SeededRng>;

fn kernel(disk: &SharedIo, clock: &Rc<SimClock>) -> K {
    let config = Config {
        dir: "wal".into(),
        segment_limit: 400,
        node: 1,
    };
    Kernel::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(5),
        &config,
    )
    .unwrap()
    .0
}

fn fresh() -> (SharedIo, Rc<SimClock>) {
    (
        SharedIo::new(SimIo::new(1, FaultProfile::clean())),
        Rc::new(SimClock::new(1_000)),
    )
}

fn fill(k: &mut K, n: usize) {
    for _ in 0..n {
        let id = k.new_id();
        k.commit(ChangeSet::new(id, vec![1; 40])).unwrap();
    }
}

#[test]
fn inv_i9_a_healthy_log_is_clean_and_the_kernel_agrees_with_a_rebuild() {
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    k.begin_turn(TurnRef {
        session: 1,
        turn: 1,
    })
    .unwrap();
    fill(&mut k, 12);
    let report = k.scrub().unwrap();
    assert!(report.is_clean(), "{:?}", report.issues);
    assert_eq!(report.change_sets, 12);
    assert!(
        report.segments > 1,
        "the small segment limit should have rotated"
    );
    assert_eq!(report.rebuilt_turns.get(&1), Some(&1));
    // The standalone scan of the same directory gives the same answer.
    assert_eq!(scrub(&disk, "wal").unwrap().change_sets, 12);
}

#[test]
fn damage_is_reported_by_kind() {
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    fill(&mut k, 6);
    drop(k);
    let segs = disk.list("wal").unwrap();
    let last = segs
        .iter()
        .filter(|n| n.ends_with(".seg"))
        .max()
        .unwrap()
        .clone();
    let first = segs
        .iter()
        .filter(|n| n.ends_with(".seg"))
        .min()
        .unwrap()
        .clone();

    // Garbage at the end of the newest segment is a torn tail, not damage.
    let mut io = disk.clone();
    io.append(&format!("wal/{last}"), &[0xAB; 9]).unwrap();
    let r = scrub(&disk, "wal").unwrap();
    assert!(
        r.is_clean(),
        "a torn tail alone is expected after a crash: {:?}",
        r.issues
    );
    assert!(
        r.issues
            .iter()
            .any(|i| matches!(i, Issue::TornTail { bytes: 9, .. }))
    );
    io.truncate(
        &format!("wal/{last}"),
        io.len(&format!("wal/{last}")).unwrap() - 9,
    )
    .unwrap();

    // A flipped byte in the oldest segment, with more log after it, is mid-log damage.
    let path = format!("wal/{first}");
    let mut bytes = io.read(&path).unwrap();
    bytes[40] ^= 0xFF;
    io.truncate(&path, 0).unwrap();
    io.append(&path, &bytes).unwrap();
    let r = scrub(&disk, "wal").unwrap();
    assert!(!r.is_clean());
    assert!(
        r.issues
            .iter()
            .any(|i| matches!(i, Issue::MidLogDamage { .. } | Issue::MissingSegment { .. })),
        "{:?}",
        r.issues
    );
}

fn with_bug(bug: Bug, check: impl FnOnce(&SharedIo, &Rc<SimClock>)) {
    let (disk, clock) = fresh();
    bugs::enable(bug);
    check(&disk, &clock);
    bugs::clear();
}

#[test]
fn each_planted_invariant_bug_shows_up_in_the_scrub() {
    with_bug(Bug::NoDedupe, |disk, clock| {
        let mut k = kernel(disk, clock);
        let id = k.new_id();
        k.commit(ChangeSet::new(id, vec![1])).unwrap();
        k.commit(ChangeSet::new(id, vec![1])).unwrap();
        assert!(
            k.scrub()
                .unwrap()
                .issues
                .iter()
                .any(|i| matches!(i, Issue::DuplicateId { .. }))
        );
    });
    with_bug(Bug::HlcRegress, |disk, clock| {
        let mut k = kernel(disk, clock);
        fill(&mut k, 2);
        clock.set(5);
        fill(&mut k, 2);
        assert!(
            k.scrub()
                .unwrap()
                .issues
                .iter()
                .any(|i| matches!(i, Issue::HlcNotIncreasing { .. }))
        );
    });
    with_bug(Bug::FenceIgnored, |disk, clock| {
        let mut k = kernel(disk, clock);
        k.begin_turn(TurnRef {
            session: 1,
            turn: 1,
        })
        .unwrap();
        k.begin_turn(TurnRef {
            session: 1,
            turn: 2,
        })
        .unwrap();
        let mut stale = ChangeSet::new(k.new_id(), vec![1]);
        stale.turn = Some(TurnRef {
            session: 1,
            turn: 1,
        });
        k.commit(stale).unwrap();
        assert!(
            k.scrub()
                .unwrap()
                .issues
                .iter()
                .any(|i| matches!(i, Issue::FencedWriteLogged { .. }))
        );
    });
    with_bug(Bug::DedupeNotRebuilt, |disk, clock| {
        let mut k = kernel(disk, clock);
        fill(&mut k, 3);
        drop(k);
        let k2 = kernel(disk, clock);
        let issues = k2.scrub().unwrap().issues;
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, Issue::DerivedStateMismatch { .. })),
            "{issues:?}"
        );
    });
    with_bug(Bug::FenceNotRebuilt, |disk, clock| {
        let mut k = kernel(disk, clock);
        k.begin_turn(TurnRef {
            session: 2,
            turn: 3,
        })
        .unwrap();
        drop(k);
        let k2 = kernel(disk, clock);
        assert!(
            k2.scrub()
                .unwrap()
                .issues
                .iter()
                .any(|i| matches!(i, Issue::DerivedStateMismatch { .. }))
        );
    });
}

/// At the log level, a change set is all there or not there: cutting the log inside the last
/// record leaves the earlier ones whole and the cut one absent, never half of it.
#[test]
fn inv_i2_a_log_cut_inside_a_record_shows_whole_change_sets_only() {
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    fill(&mut k, 3);
    drop(k);
    let seg = "wal/00000000000000000001.seg";
    let bytes = disk.read(seg).unwrap();
    for cut in 1..30 {
        let mut d = disk.clone();
        d.truncate(seg, (bytes.len() - cut) as u64).unwrap();
        let (reopened, _) = Kernel::open(
            d.clone(),
            SharedClock(clock.clone()),
            SeededRng::new(6),
            &Config {
                dir: "wal".into(),
                segment_limit: 400,
                node: 1,
            },
        )
        .unwrap();
        let records = reopened.read_from(1, usize::MAX).unwrap().0;
        let sets: Vec<_> = records
            .iter()
            .filter_map(|r| ChangeSet::decode(&r.payload).ok())
            .collect();
        assert_eq!(sets.len(), 2, "cut of {cut} bytes");
        assert!(sets.iter().all(|c| c.payload == vec![1; 40]));
        // Restore the full log for the next cut.
        d.truncate(seg, 0).unwrap();
        d.append(seg, &bytes).unwrap();
    }
}

#[test]
fn a_checkpoint_is_found_on_recovery_and_a_wrong_one_is_refused() {
    use lattice_core::Log;
    use lattice_core::changeset::{Checkpoint, encode_checkpoint};
    use lattice_core::record::RecordType;
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    fill(&mut k, 3);
    let lsn = k.checkpoint().unwrap();
    assert_eq!(lsn, 4);
    drop(k);
    let config = Config {
        dir: "wal".into(),
        segment_limit: 400,
        node: 1,
    };
    let open = |d: &SharedIo| {
        Kernel::open(
            d.clone(),
            SharedClock(clock.clone()),
            SeededRng::new(8),
            &config,
        )
    };
    let (k, rec) = open(&disk).unwrap();
    assert_eq!(rec.checkpoint.map(|c| c.change_sets), Some(3));
    drop(k);
    assert!(scrub(&disk, "wal").unwrap().is_clean());

    // A checkpoint that claims one change set too many means the log lost something.
    let (mut log, _) = Log::open(disk.clone(), "wal", 400).unwrap();
    let wrong = Checkpoint {
        through_lsn: 4,
        change_sets: 4,
        state_crc: 0,
    };
    log.append(RecordType::Checkpoint as u8, &encode_checkpoint(wrong))
        .unwrap();
    log.sync().unwrap();
    drop(log);
    assert!(open(&disk).is_err());
    assert!(!scrub(&disk, "wal").unwrap().is_clean());
}

/// A checkpoint is wrong if any one of its three claims is wrong, however right the other two are.
#[test]
fn a_checkpoint_is_flagged_when_only_its_change_set_count_is_wrong() {
    use lattice_core::Log;
    use lattice_core::changeset::{Checkpoint, encode_checkpoint, state_crc};
    use lattice_core::record::RecordType;
    use std::collections::{BTreeMap, BTreeSet};
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    let mut ids = BTreeSet::new();
    for _ in 0..3 {
        let id = k.new_id();
        k.commit(ChangeSet::new(id, vec![1; 20])).unwrap();
        ids.insert(id);
    }
    drop(k);
    // The position and the state checksum are right; only the count is not.
    let (mut log, _) = Log::open(disk.clone(), "wal", 400).unwrap();
    let wrong_count = Checkpoint {
        through_lsn: 3,
        change_sets: 99,
        state_crc: state_crc(&ids, &BTreeMap::new()),
    };
    log.append(
        RecordType::Checkpoint as u8,
        &encode_checkpoint(wrong_count),
    )
    .unwrap();
    log.sync().unwrap();
    drop(log);
    let report = scrub(&disk, "wal").unwrap();
    assert!(
        report
            .issues
            .iter()
            .any(|i| matches!(i, Issue::DerivedStateMismatch { .. })),
        "{:?}",
        report.issues
    );
}
