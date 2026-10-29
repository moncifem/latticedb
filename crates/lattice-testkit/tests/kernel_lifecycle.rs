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
    let (mut log, _) = Log::open(disk.clone(), "wal", 400).unwrap();
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

#[test]
fn settle_recovers_the_reservation_of_a_dropped_transaction() {
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    let id = k.new_id();
    let t = k
        .begin(ChangeSet::new(id, vec![9]))
        .validate(&AllowAll)
        .unwrap()
        .authorize(&AllowAll)
        .unwrap();
    let PrepareOutcome::Ready(t) = k.prepare(t).unwrap() else {
        panic!("duplicate")
    };
    drop(k.log(t).unwrap()); // the caller loses the transaction
    let again = ChangeSet::new(id, vec![9]);
    assert!(matches!(
        k.commit(again.clone()),
        Err(TxnError::InFlight(_))
    ));
    assert!(matches!(k.checkpoint(), Err(TxnError::Invalid(_))));
    assert_eq!(k.settle().unwrap(), 1);
    // The record is in the log, so the id now counts as committed.
    assert!(matches!(
        k.commit(again),
        Ok(lattice_core::Receipt::Duplicate)
    ));
    k.checkpoint().unwrap();
    assert_eq!(k.settle().unwrap(), 0);
}

#[test]
fn a_dropped_valid_transaction_next_to_a_foreign_one_is_settled_not_leaked() {
    let (disk_a, clock) = fresh();
    let (disk_b, _) = fresh();
    let mut a = kernel(&disk_a, &clock);
    let mut b = kernel(&disk_b, &clock);
    let valid = logged(&mut a, 1);
    let foreign = logged(&mut b, 2);
    assert!(a.sync(vec![valid, foreign]).is_err());
    assert_eq!(a.settle().unwrap(), 1);
    a.checkpoint().unwrap();
}

#[test]
fn a_scrub_of_a_tail_full_of_look_alike_headers_terminates() {
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    let id = k.new_id();
    k.commit(ChangeSet::new(id, vec![1; 40])).unwrap();
    drop(k);
    let seg = "wal/00000000000000000001.seg";
    let mut io = disk.clone();
    // 2 MiB of a pattern that looks like a header with a 65,536-byte length at every position.
    let junk: Vec<u8> = [0u8, 0, 1, 0]
        .iter()
        .copied()
        .cycle()
        .take(2 << 20)
        .collect();
    io.append(seg, &junk).unwrap();
    // The previous scan decoded at every byte and took minutes here; completing is the assertion.
    let report = scrub(&disk, "wal").unwrap();
    assert!(!report.issues.is_empty());
}

#[test]
fn a_full_disk_refuses_commits_without_losing_or_inventing_acknowledged_ones() {
    let profile = FaultProfile {
        disk_full_after: Some(1500),
        ..FaultProfile::clean()
    };
    let disk = SharedIo::new(SimIo::new(1, profile));
    let clock = Rc::new(SimClock::new(1_000));
    let mut k = kernel(&disk, &clock);
    let (mut acked, mut refused) = (Vec::new(), 0);
    for _ in 0..200 {
        let id = k.new_id();
        match k.commit(ChangeSet::new(id, vec![7; 60])) {
            Ok(_) => acked.push(id),
            Err(_) => refused += 1,
        }
        if k.is_poisoned() {
            break;
        }
    }
    assert!(
        !acked.is_empty() && refused > 0,
        "the disk never filled: {}",
        acked.len()
    );
    drop(k);
    // Whatever the kernel acknowledged is there after a restart, with no space problem now.
    disk.set_profile(FaultProfile::clean());
    let (reopened, _) = Kernel::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(1),
        &config(),
    )
    .unwrap();
    let records = reopened.read_from(1, usize::MAX).unwrap().0;
    let on_disk: Vec<_> = records
        .iter()
        .filter_map(|r| ChangeSet::decode(&r.payload).ok())
        .map(|c| c.id)
        .collect();
    for id in &acked {
        assert!(on_disk.contains(id), "an acknowledged change set vanished");
    }
    assert!(scrub(&disk, "wal").unwrap().is_clean());
}

#[test]
fn default_recovery_metadata_and_cursor_agree_with_streaming_trace() {
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    k.begin_turn(TurnRef {
        session: 3,
        turn: 1,
    })
    .unwrap();
    for i in 0..9 {
        let id = k.new_id();
        k.commit(ChangeSet::new(id, vec![i; 30])).unwrap();
    }
    k.checkpoint().unwrap();
    drop(k);
    // A torn tail on the newest segment must be repaired the same way by both.
    let mut io = disk.clone();
    let newest = io
        .list("wal")
        .unwrap()
        .into_iter()
        .filter(|n| n.ends_with(".seg"))
        .max()
        .unwrap();
    io.append(&format!("wal/{newest}"), &[0xCD; 7]).unwrap();

    let mut streamed = Vec::new();
    let (k1, report1, cp1) = Kernel::open_streaming(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(1),
        &config(),
        |r| streamed.push(r),
    )
    .unwrap();
    drop(k1);
    let (reopened, rec) = Kernel::open(
        disk.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(1),
        &config(),
    )
    .unwrap();
    let records = reopened.read_from(1, usize::MAX).unwrap().0;
    assert_eq!(streamed.len(), records.len());
    assert!(
        streamed
            .iter()
            .zip(&records)
            .all(|(a, b)| a.lsn == b.lsn && a.rtype == b.rtype && a.payload == b.payload)
    );
    assert_eq!(
        cp1.map(|c| c.change_sets),
        rec.checkpoint.map(|c| c.change_sets)
    );
    assert_eq!(report1.records, rec.report.records);
    assert!(streamed.windows(2).all(|w| w[1].lsn == w[0].lsn + 1));
}

#[test]
fn reading_notices_a_damaged_segment_header() {
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    for i in 0..12 {
        let id = k.new_id();
        k.commit(ChangeSet::new(id, vec![i; 40])).unwrap();
    }
    let mut segs: Vec<String> = disk
        .list("wal")
        .unwrap()
        .into_iter()
        .filter(|n| n.ends_with(".seg"))
        .collect();
    segs.sort();
    assert!(
        segs.len() > 2,
        "the small segment limit should have rotated"
    );
    assert!(k.read_from(1, 100).is_ok());

    // Flip a byte of the first segment's header: the reader must refuse, not skip the segment.
    let first = format!("wal/{}", segs[0]);
    let mut bytes = disk.read(&first).unwrap();
    bytes[0] ^= 0xFF;
    let mut io = disk.clone();
    io.truncate(&first, 0).unwrap();
    io.append(&first, &bytes).unwrap();
    io.sync(&first).unwrap();
    assert!(k.read_from(1, 100).is_err());

    // Restoring the byte makes the log readable again, so the refusal was about the header alone.
    bytes[0] ^= 0xFF;
    io.truncate(&first, 0).unwrap();
    io.append(&first, &bytes).unwrap();
    io.sync(&first).unwrap();
    assert!(k.read_from(1, 100).is_ok());
}

#[test]
fn recovery_reports_how_many_segments_it_scanned() {
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    for i in 0..12 {
        let id = k.new_id();
        k.commit(ChangeSet::new(id, vec![i; 40])).unwrap();
    }
    drop(k);
    let segments = disk
        .list("wal")
        .unwrap()
        .into_iter()
        .filter(|n| n.ends_with(".seg"))
        .count();
    assert!(segments > 1, "the small segment limit should have rotated");
    let (_, report) = lattice_core::Log::open(disk.clone(), "wal", 400).unwrap();
    assert_eq!(report.segments, segments);
    assert_eq!(report.records, 12);
}

#[test]
fn recovery_reports_the_bytes_it_cut_from_a_torn_tail() {
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    for i in 0..6 {
        let id = k.new_id();
        k.commit(ChangeSet::new(id, vec![i; 40])).unwrap();
    }
    drop(k);
    let newest = disk
        .list("wal")
        .unwrap()
        .into_iter()
        .filter(|n| n.ends_with(".seg"))
        .max()
        .unwrap();
    let mut io = disk.clone();
    io.append(&format!("wal/{newest}"), &[0xAB; 9]).unwrap();
    let (_, report) = lattice_core::Log::open(disk.clone(), "wal", 400).unwrap();
    assert_eq!(report.truncated_bytes, 9);
    assert_eq!(report.records, 6);
}

#[test]
fn a_segment_whose_creation_was_torn_is_rewritten_and_counted() {
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    for i in 0..6 {
        let id = k.new_id();
        k.commit(ChangeSet::new(id, vec![i; 40])).unwrap();
    }
    drop(k);
    let before = disk
        .list("wal")
        .unwrap()
        .into_iter()
        .filter(|n| n.ends_with(".seg"))
        .count();
    // The machine died while the next segment was being created: only part of its header exists.
    let mut io = disk.clone();
    io.append(&format!("wal/{:020}.seg", 7u64), &[1, 2, 3])
        .unwrap();
    let (_, report) = lattice_core::Log::open(disk.clone(), "wal", 400).unwrap();
    assert_eq!(report.truncated_bytes, 3);
    assert_eq!(report.segments, before + 1);
    assert_eq!(report.records, 6);
}

#[test]
fn a_write_for_a_turn_that_has_not_begun_is_refused_as_unknown_not_fenced() {
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    k.begin_turn(TurnRef {
        session: 1,
        turn: 1,
    })
    .unwrap();
    let mut cs = ChangeSet::new(k.new_id(), vec![1]);
    cs.turn = Some(TurnRef {
        session: 1,
        turn: 2,
    });
    assert!(matches!(k.commit(cs), Err(TxnError::UnknownTurn(1))));
}

/// A clock that moves 1.5 s every time it is read, so a commit takes a whole number of those ticks.
struct Ticking(Rc<SimClock>);
impl lattice_seams::Clock for Ticking {
    fn now_ms(&self) -> u64 {
        self.0.advance(1500);
        self.0.now_ms()
    }
}

#[test]
fn commit_latency_is_recorded_in_seconds() {
    let disk = SharedIo::new(SimIo::new(1, FaultProfile::clean()));
    let (mut k, _) = Kernel::open(
        disk.clone(),
        Ticking(Rc::new(SimClock::new(1_000))),
        SeededRng::new(1),
        &config(),
    )
    .unwrap();
    let id = k.new_id();
    k.commit(ChangeSet::new(id, vec![1])).unwrap();
    let h = k
        .metrics()
        .histogram(lattice_core::metrics::names::COMMIT_LATENCY_SECONDS)
        .unwrap();
    assert_eq!(h.count, 1);
    // Every clock read adds 1.5 s, so the recorded latency is a positive whole number of ticks. A
    // wrong unit conversion gives a fraction of a tick, zero or a number in the millions.
    let ticks = h.sum / 1.5;
    assert!(
        (ticks - ticks.round()).abs() < 1e-9 && (1.0..60.0).contains(&ticks),
        "recorded {} s",
        h.sum
    );
}

#[test]
fn the_kernel_knows_which_ids_it_has_committed() {
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    let id = k.new_id();
    let other = k.new_id();
    assert!(!k.contains(&id));
    k.commit(ChangeSet::new(id, vec![1])).unwrap();
    assert!(k.contains(&id));
    assert!(!k.contains(&other));
}

#[test]
fn a_last_segment_as_long_as_a_header_but_not_one_is_corruption_not_a_torn_creation() {
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    // Two small commits stay inside the first segment, so the next segment does not exist yet.
    for i in 0..2 {
        let id = k.new_id();
        k.commit(ChangeSet::new(id, vec![i; 40])).unwrap();
    }
    let path = format!("wal/{:020}.seg", k.durable_lsn() + 1);
    drop(k);
    assert!(
        !disk.exists(&path),
        "the garbage must be a new segment file"
    );
    // A header cut short by a crash is shorter than a header. These bytes are as long as one and are
    // not one, so recovery must refuse the log rather than rewrite the segment.
    let mut io = disk.clone();
    io.append(&path, &[0xFF; lattice_core::record::SEGMENT_HEADER_LEN])
        .unwrap();
    assert!(lattice_core::Log::open(disk.clone(), "wal", 400).is_err());
}

#[test]
fn a_transaction_from_a_kernel_with_another_owner_is_refused_at_prepare() {
    let (disk_a, clock) = fresh();
    let (disk_b, _) = fresh();
    let mut a = kernel(&disk_a, &clock);
    let (mut b, _) = Kernel::open(
        disk_b.clone(),
        SharedClock(clock.clone()),
        SeededRng::new(99),
        &config(),
    )
    .unwrap();
    let id = a.new_id();
    let txn = a
        .begin(ChangeSet::new(id, vec![1]))
        .validate(&AllowAll)
        .unwrap()
        .authorize(&AllowAll)
        .unwrap();
    assert!(matches!(b.prepare(txn), Err(TxnError::ForeignTransaction)));
}

/// A checkpoint makes three claims: its position, how many change sets precede it, and a checksum of
/// the state. Recovery must refuse it if any one of them is wrong, however right the other two are.
#[test]
fn recovery_refuses_a_checkpoint_when_any_one_claim_is_wrong() {
    use lattice_core::changeset::state_crc;
    use std::collections::{BTreeMap, BTreeSet};
    for case in 0..4 {
        let (disk, clock) = fresh();
        let mut k = kernel(&disk, &clock);
        let mut ids = BTreeSet::new();
        for _ in 0..3 {
            let id = k.new_id();
            k.commit(ChangeSet::new(id, vec![1; 20])).unwrap();
            ids.insert(id);
        }
        drop(k);
        let good = Checkpoint {
            through_lsn: 3,
            change_sets: 3,
            state_crc: state_crc(&ids, &BTreeMap::new()),
        };
        let checkpoint = match case {
            0 => good,
            1 => Checkpoint {
                through_lsn: 7,
                ..good
            },
            2 => Checkpoint {
                change_sets: 99,
                ..good
            },
            _ => Checkpoint {
                state_crc: good.state_crc ^ 1,
                ..good
            },
        };
        let (mut log, _) = Log::open(disk.clone(), "wal", 400).unwrap();
        log.append(RecordType::Checkpoint as u8, &encode_checkpoint(checkpoint))
            .unwrap();
        log.sync().unwrap();
        drop(log);
        let opened = Kernel::open(
            disk.clone(),
            SharedClock(clock.clone()),
            SeededRng::new(5),
            &config(),
        );
        assert_eq!(opened.is_err(), case != 0, "case {case}");
    }
}

#[test]
fn the_ledger_lives_in_its_own_directory_and_does_not_rotate_early() {
    let (disk, clock) = fresh();
    for n in 0..6u64 {
        lattice_core::ledger::append(
            disk.clone(),
            SharedClock(clock.clone()),
            SeededRng::new(n + 1),
            "sim",
            "{}",
        )
        .unwrap();
    }
    let segments = disk
        .list("ledger")
        .unwrap()
        .into_iter()
        .filter(|n| n.ends_with(".seg"))
        .count();
    assert_eq!(segments, 1, "six small entries fit in one segment");
}

#[test]
fn queued_change_sets_get_consecutive_tickets() {
    use lattice_core::ChangeSetId;
    use lattice_core::pipeline::{Pipeline, Ticket};
    let (disk, clock) = fresh();
    let mut pipeline = Pipeline::new(kernel(&disk, &clock), 8, 4);
    let tickets: Vec<Ticket> = (0..3u64)
        .map(|n| {
            pipeline
                .submit(ChangeSet::new(ChangeSetId::from_parts(1, n, 0), vec![1]))
                .unwrap()
        })
        .collect();
    assert_eq!(tickets, vec![Ticket(0), Ticket(1), Ticket(2)]);
}

#[test]
fn the_scrub_does_not_accept_a_valid_record_that_is_out_of_sequence() {
    use lattice_core::record::{Decoded, SEGMENT_HEADER_LEN, decode_record};
    let (disk, clock) = fresh();
    let mut k = kernel(&disk, &clock);
    for _ in 0..2 {
        let id = k.new_id();
        k.commit(ChangeSet::new(id, vec![1; 20])).unwrap();
    }
    drop(k);
    let path = "wal/00000000000000000001.seg";
    let data = disk.read(path).unwrap();
    let Decoded::Record(_, end) = decode_record(&data, SEGMENT_HEADER_LEN) else {
        panic!("the first record should decode");
    };
    // The first record's bytes again, after the second: valid on their own, wrong in this place.
    let mut io = disk.clone();
    io.append(path, &data[SEGMENT_HEADER_LEN..end]).unwrap();
    assert_eq!(scrub(&disk, "wal").unwrap().records, 2);
}

#[test]
fn a_full_simulated_disk_counts_each_refused_append() {
    let mut io = SimIo::new(
        1,
        FaultProfile {
            disk_full_after: Some(10),
            ..FaultProfile::clean()
        },
    );
    assert_eq!(io.no_space_events(), 0);
    assert!(io.append("f", &[0u8; 100]).is_err());
    assert_eq!(io.no_space_events(), 1);
    assert!(io.append("f", &[0u8; 100]).is_err());
    assert_eq!(io.no_space_events(), 2);
}
