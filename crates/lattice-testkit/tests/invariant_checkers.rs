//! The invariant checkers, one test each way: every checker reports under its own id, flags a planted
//! violation and stays quiet on a clean log. Mutation testing showed the ids and a few boundaries were
//! not pinned, so a checker could be weakened or renamed without a test noticing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lattice_abi::Hlc;
use lattice_core::changeset::{ChangeSet, ChangeSetId, TurnRef, encode_turn_begin};
use lattice_core::record::{Record, RecordType};
use lattice_testkit::invariants::{
    AckedDurable, HlcMonotonic, Invariant, LogView, NoDuplicateIds, Oracle, TurnsFenced,
    WholeContiguousRecords, check_all, default_invariants,
};

fn cs(id: u128, wall: u64, turn: Option<TurnRef>) -> ChangeSet {
    let mut c = ChangeSet::new(ChangeSetId::from_raw(id), vec![id as u8]);
    c.hlc = Hlc {
        wall_ms: wall,
        counter: 0,
        node: 1,
    };
    c.turn = turn;
    c
}

fn view(change_sets: Vec<(u64, ChangeSet)>, turn_begins: Vec<(u64, TurnRef)>) -> LogView {
    let mut lsns: Vec<u64> = change_sets
        .iter()
        .map(|(l, _)| *l)
        .chain(turn_begins.iter().map(|(l, _)| *l))
        .collect();
    lsns.sort_unstable();
    LogView {
        lsns,
        change_sets,
        turn_begins,
        undecodable: 0,
    }
}

#[test]
fn every_checker_reports_under_its_own_id() {
    let ids: Vec<&str> = default_invariants().iter().map(|i| i.id()).collect();
    assert_eq!(ids, ["I1", "I2", "I4", "I6", "I12"]);
    assert_eq!(AckedDurable.id(), "I1");
    assert_eq!(WholeContiguousRecords.id(), "I2");
    assert_eq!(HlcMonotonic.id(), "I4");
    assert_eq!(NoDuplicateIds.id(), "I6");
    assert_eq!(TurnsFenced.id(), "I12");
}

#[test]
fn each_checker_flags_its_violation_under_its_id() {
    let oracle = Oracle::default();
    let mut missing = Oracle::default();
    missing.acked.insert(
        ChangeSetId::from_raw(9),
        lattice_testkit::invariants::Expected {
            payload: vec![9],
            author: 0,
            turn: None,
            parents: vec![],
        },
    );
    let gap = LogView {
        lsns: vec![1, 3],
        ..Default::default()
    };
    let undecodable = LogView {
        lsns: vec![1],
        undecodable: 2,
        ..Default::default()
    };
    let backwards = view(vec![(1, cs(1, 5, None)), (2, cs(2, 4, None))], vec![]);
    let equal = view(vec![(1, cs(1, 5, None)), (2, cs(2, 5, None))], vec![]);
    let duplicate = view(vec![(1, cs(1, 1, None)), (2, cs(1, 2, None))], vec![]);
    let unfenced = view(
        vec![(
            2,
            cs(
                1,
                1,
                Some(TurnRef {
                    session: 1,
                    turn: 1,
                }),
            ),
        )],
        vec![],
    );
    for (name, found) in [
        ("I1", AckedDurable.check(&LogView::default(), &missing)),
        ("I2", WholeContiguousRecords.check(&gap, &oracle)),
        ("I2", WholeContiguousRecords.check(&undecodable, &oracle)),
        ("I4", HlcMonotonic.check(&backwards, &oracle)),
        ("I4", HlcMonotonic.check(&equal, &oracle)),
        ("I6", NoDuplicateIds.check(&duplicate, &oracle)),
        ("I12", TurnsFenced.check(&unfenced, &oracle)),
    ] {
        assert!(!found.is_empty(), "{name} found nothing");
        assert!(
            found.iter().all(|v| v.invariant == name),
            "{name}: {found:?}"
        );
    }
}

#[test]
fn a_clean_log_passes_every_checker() {
    let t = TurnRef {
        session: 1,
        turn: 2,
    };
    let clean = view(
        vec![(1, cs(1, 1, None)), (3, cs(2, 2, Some(t)))],
        vec![(2, t)],
    );
    assert!(check_all(&clean, &Oracle::default()).is_empty());
}

#[test]
fn a_write_in_the_same_position_as_its_turn_begin_is_not_after_it() {
    // The turn begin must be strictly earlier than the write that depends on it.
    let t = TurnRef {
        session: 1,
        turn: 1,
    };
    let same = view(vec![(5, cs(1, 1, Some(t)))], vec![(5, t)]);
    assert_eq!(TurnsFenced.check(&same, &Oracle::default()).len(), 1);
    let before = view(vec![(6, cs(1, 1, Some(t)))], vec![(5, t)]);
    assert!(TurnsFenced.check(&before, &Oracle::default()).is_empty());
}

#[test]
fn the_contiguity_check_reports_the_first_gap_only() {
    let v = LogView {
        lsns: vec![1, 2, 5, 9],
        ..Default::default()
    };
    let found = WholeContiguousRecords.check(&v, &Oracle::default());
    assert_eq!(found.len(), 1);
    assert!(
        found[0].detail.contains("lsn 5 at position 2"),
        "{}",
        found[0].detail
    );
}

fn record(lsn: u64, rtype: RecordType, payload: Vec<u8>) -> Record {
    Record {
        lsn,
        rtype: rtype as u8,
        payload,
    }
}

#[test]
fn decoding_records_counts_each_kind_once() {
    let t = TurnRef {
        session: 1,
        turn: 1,
    };
    let records = vec![
        record(1, RecordType::ChangeSet, cs(1, 1, None).encode().unwrap()),
        record(2, RecordType::ChangeSet, vec![0xff]), // undecodable change set
        record(3, RecordType::TurnBegin, encode_turn_begin(t)),
        record(4, RecordType::TurnBegin, vec![1, 2]), // undecodable turn begin
        record(5, RecordType::Checkpoint, vec![]),
        Record {
            lsn: 6,
            rtype: 200,
            payload: vec![],
        }, // unknown type
    ];
    let v = LogView::from_records(&records);
    assert_eq!(v.lsns, vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(v.change_sets.len(), 1);
    assert_eq!(v.turn_begins, vec![(3, t)]);
    assert_eq!(v.undecodable, 3);
}
