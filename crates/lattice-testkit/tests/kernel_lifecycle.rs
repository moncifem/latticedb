//! The public transaction lifecycle at its edges: foreign transactions, checkpoints taken mid-flight,
//! damaged reads and malformed records. Each test pins a failure an independent review found.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use lattice_core::changeset::{ChangeSet, Checkpoint, TurnRef, encode_checkpoint};
use lattice_core::record::RecordType;
use lattice_core::scrub::{Issue, scrub};
use lattice_core::txn::TxnError;
use lattice_core::{AllowAll, Config, Kernel, Log, PrepareOutcome};
use lattice_seams::Io;
use lattice_sim::{FaultProfile, SeededRng, SimClock, SimIo};
use lattice_testkit::scenario::{SharedClock, SharedIo};
use std::rc::Rc;

type K = Kernel<SharedIo, SharedClock, SeededRng>;

fn config() -> Config {
    Config {
        dir: "wal".into(),
        segment_limit: 400,
        node: 1,
    }
}

fn fresh() -> (SharedIo, Rc<SimClock>) {
    (
        SharedIo::new(SimIo::new(1, FaultProfile::clean())),
        Rc::new(SimClock::new(1_000)),
    )
}

/// Every kernel here gets the same RNG seed on purpose: the owner token is drawn from the RNG, so
/// kernels on different disks share it and must still not accept each other's transactions.
fn kernel(disk: &SharedIo, clock: &Rc<SimClock>) -> K {
    Kernel::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(1),
        &config(),
    )
    .unwrap()
    .0
}

fn logged(k: &mut K, payload: u8) -> lattice_core::Txn<lattice_core::Logged> {
    let id = k.new_id();
    let t = k
        .begin(ChangeSet::new(id, vec![payload]))
        .validate(&AllowAll)
        .unwrap()
        .authorize(&AllowAll)
        .unwrap();
    match k.prepare(t).unwrap() {
        PrepareOutcome::Ready(t) => k.log(t).unwrap(),
        PrepareOutcome::Duplicate(_) => panic!("fresh id was a duplicate"),
    }
}

#[test]
fn a_transaction_from_another_kernel_with_the_same_owner_is_refused() {
    let (disk_a, clock) = fresh();
    let (disk_b, _) = fresh();
    let mut a = kernel(&disk_a, &clock);
    let mut b = kernel(&disk_b, &clock);
    let x = logged(&mut a, 1);
    // B logs something unrelated at the same LSN.
    let y_id = b.new_id();
    b.commit(ChangeSet::new(y_id, vec![2])).unwrap();
    assert!(matches!(b.sync(vec![x]), Err(TxnError::ForeignTransaction)));
}

#[test]
fn a_transaction_with_the_same_id_and_lsn_but_other_content_is_refused() {
    let (disk_a, clock) = fresh();
    let (disk_b, _) = fresh();
    let mut a = kernel(&disk_a, &clock);
    let mut b = kernel(&disk_b, &clock);
    // Same seed, so the same first id: log different payloads at the same LSN and leave both in
    // flight. Owner, id and LSN all match; only the content differs.
    let x = logged(&mut a, 1);
    let y = logged(&mut b, 2);
    assert_eq!(x.id(), y.id());
    assert!(matches!(b.sync(vec![x]), Err(TxnError::ForeignTransaction)));
    assert!(b.sync(vec![y]).is_ok());
}

#[test]
fn a_checkpoint_is_refused_while_a_change_set_is_in_flight() {
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    let t = logged(&mut k, 1);
    assert!(matches!(k.checkpoint(), Err(TxnError::Invalid(_))));
    let durable = k.sync(vec![t]).unwrap().pop().unwrap();
    // Durable but not applied is still in flight.
    assert!(matches!(k.checkpoint(), Err(TxnError::Invalid(_))));
    k.apply(durable).unwrap();
    k.checkpoint().unwrap();
    drop(k);
    // The log, with the checkpoint after the change set, recovers.
    let (_, rec) = Kernel::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(1),
        &config(),
    )
    .unwrap();
    assert_eq!(rec.checkpoint.map(|c| c.change_sets), Some(1));
}

#[test]
fn reading_a_damaged_log_is_an_error_not_a_short_history() {
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    for i in 0..3 {
        let id = k.new_id();
        k.commit(ChangeSet::new(id, vec![i; 40])).unwrap();
    }
    let seg = "wal/00000000000000000001.seg";
    let mut bytes = disk.read(seg).unwrap();
    // Damage the second record, past the 8-byte segment header and the first record.
    let target = bytes.len() / 2;
    bytes[target] ^= 0xFF;
    let mut io = disk.clone();
    io.truncate(seg, 0).unwrap();
    io.append(seg, &bytes).unwrap();
    io.sync(seg).unwrap();
    assert!(k.read_from(1, 10).is_err());
}

fn append_raw(disk: &SharedIo, rtype: RecordType, payload: &[u8]) {
    let (mut log, _, _) = Log::open(disk.clone(), "wal", 400).unwrap();
    log.append(rtype as u8, payload).unwrap();
    log.sync().unwrap();
}

#[test]
fn a_write_for_a_turn_that_never_began_is_flagged_by_the_scrub() {
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    let id = k.new_id();
    drop(k);
    let mut cs = ChangeSet::new(id, vec![1]);
    cs.turn = Some(TurnRef {
        session: 9,
        turn: 99,
    });
    cs.hlc = lattice_abi::Hlc::default().max(cs.hlc);
    append_raw(&disk, RecordType::ChangeSet, &cs.encode().unwrap());
    let report = scrub(&disk, "wal").unwrap();
    assert!(
        report
            .issues
            .iter()
            .any(|i| matches!(i, Issue::FencedWriteLogged { .. })),
        "{:?}",
        report.issues
    );
}

#[test]
fn a_checkpoint_claiming_the_last_lsn_is_reported_not_a_panic() {
    let (disk, clock) = fresh();
    let k = kernel(&disk, &clock);
    drop(k);
    append_raw(
        &disk,
        RecordType::Checkpoint,
        &encode_checkpoint(Checkpoint {
            through_lsn: u64::MAX,
            change_sets: 0,
            state_crc: 0,
        }),
    );
    assert!(!scrub(&disk, "wal").unwrap().is_clean());
    assert!(
        Kernel::open(
            disk.clone(),
            SharedClock(clock.clone()),
            SeededRng::new(1),
            &config(),
        )
        .is_err()
    );
}

#[test]
fn a_log_with_its_first_segment_deleted_cannot_be_read() {
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    for i in 0..8 {
        let id = k.new_id();
        k.commit(ChangeSet::new(id, vec![i; 40])).unwrap();
    }
    let mut io = disk.clone();
    let mut segs: Vec<String> = io
        .list("wal")
        .unwrap()
        .into_iter()
        .filter(|n| n.ends_with(".seg"))
        .collect();
    segs.sort();
    assert!(
        segs.len() > 1,
        "the small segment limit should have rotated"
    );
    io.remove(&format!("wal/{}", segs[0])).unwrap();
    assert!(k.read_from(1, 100).is_err());
    for s in &segs[1..] {
        io.remove(&format!("wal/{s}")).unwrap();
    }
    assert!(k.read_from(1, 100).is_err());
}
