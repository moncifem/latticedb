//! Issue 442: recovery against power-loss shapes that are not a prefix of the unsynced bytes.
//!
//! The expected outcome of every case is derived from `formats/wal-v1.md` and from the bytes the
//! test itself wrote, never from what recovery returned: the test knows each frame it appended, so it
//! can tell from the surviving file which frames are intact and apply the documented rule.
//!
//! * The first frame that is not intact marks the damage. If a later frame is intact, recovery must
//!   refuse and leave the file alone (the documented fail-safe). If none is, the damage is a torn
//!   tail and recovery must truncate exactly there and return every record before it.
//! * A newest segment shorter than its header is a torn creation and is rewritten. A newest segment
//!   with a complete but invalid header (a zero-filled extent) is refused and left untouched; the
//!   documented operator procedure (remove the empty segment) must then make the log open again.
//! * In no case may a record that was synced before the cut be missing, duplicated or reordered.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use crate::record::{encode_record, segment_header};
use crate::{Log, LogError};
use lattice_seams::{Io, IoError};
use lattice_sim::{FaultProfile, SimIo};
use std::{cell::RefCell, rc::Rc};

const SEG1: &str = "wal/00000000000000000001.seg";
const SEG3: &str = "wal/00000000000000000003.seg";

#[derive(Clone)]
struct Disk(Rc<RefCell<SimIo>>);

impl Io for Disk {
    fn exists(&self, p: &str) -> bool {
        self.0.borrow().exists(p)
    }
    fn len(&self, p: &str) -> Result<u64, IoError> {
        self.0.borrow().len(p)
    }
    fn read(&self, p: &str) -> Result<Vec<u8>, IoError> {
        self.0.borrow().read(p)
    }
    fn read_prefix(&self, p: &str, n: usize) -> Result<Vec<u8>, IoError> {
        self.0.borrow().read_prefix(p, n)
    }
    fn append(&mut self, p: &str, b: &[u8]) -> Result<(), IoError> {
        self.0.borrow_mut().append(p, b)
    }
    fn truncate(&mut self, p: &str, n: u64) -> Result<(), IoError> {
        self.0.borrow_mut().truncate(p, n)
    }
    fn sync(&mut self, p: &str) -> Result<(), IoError> {
        self.0.borrow_mut().sync(p)
    }
    fn sync_dir(&mut self, p: &str) -> Result<(), IoError> {
        self.0.borrow_mut().sync_dir(p)
    }
    fn list(&self, p: &str) -> Result<Vec<String>, IoError> {
        self.0.borrow().list(p)
    }
    fn remove(&mut self, p: &str) -> Result<(), IoError> {
        self.0.borrow_mut().remove(p)
    }
    fn create_dir_all(&mut self, p: &str) -> Result<(), IoError> {
        self.0.borrow_mut().create_dir_all(p)
    }
    fn lock(&mut self, p: &str) -> Result<(), IoError> {
        self.0.borrow_mut().lock(p)
    }
    fn unlock(&mut self, p: &str) {
        self.0.borrow_mut().unlock(p)
    }
}

/// Power loss shapes only: no transient faults, no rot, and the directory entries survive so that
/// what is tested is the content of the files.
fn shapes() -> FaultProfile {
    FaultProfile {
        lose_unsynced_dirs: false,
        ..FaultProfile::non_prefix()
    }
}

fn disk(seed: u64) -> Disk {
    Disk(Rc::new(RefCell::new(SimIo::new(seed, shapes()))))
}

/// Writes `bytes` to `path` and makes them durable, as a finished and fsynced segment.
fn plant(d: &mut Disk, path: &str, bytes: &[u8]) {
    d.create_dir_all("wal").unwrap();
    d.sync_dir("").unwrap();
    d.append(path, bytes).unwrap();
    d.sync(path).unwrap();
    d.sync_dir("wal").unwrap();
}

fn payload(lsn: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (lsn as usize * 31 + i) as u8 | 1)
        .collect()
}

fn frame(lsn: u64, len: usize) -> Vec<u8> {
    encode_record(lsn, 1, &payload(lsn, len))
}

/// Opens the log and returns the LSNs it recovered, the bytes it truncated and the log itself.
fn open(d: &Disk) -> Result<(Vec<u64>, u64, Log<Disk>), LogError> {
    let mut seen = Vec::new();
    let (log, report) = Log::open_streaming(d.clone(), "wal", 1 << 20, |r| seen.push(r.lsn))?;
    Ok((seen, report.truncated_bytes, log))
}

fn crash(d: &Disk) {
    d.0.borrow_mut().crash();
}

/// The documented operator procedure of `formats/wal-v1.md`, applied from the refusal message alone.
/// Returns false when the message is not one the procedure covers.
fn operator_repair(d: &mut Disk, why: &str) -> bool {
    let Some(rest) = why.strip_prefix("segment ") else {
        return false;
    };
    if let Some((name, tail)) = rest.split_once(" damaged at offset ")
        && let Some((at, _)) = tail.split_once(' ')
        && let Ok(at) = at.parse::<u64>()
    {
        let path = format!("wal/{name}");
        d.truncate(&path, at).unwrap();
        d.sync(&path).unwrap();
        return true;
    }
    if let Some((name, _)) = rest.split_once(" header: ") {
        let path = format!("wal/{name}");
        if d.len(&path).unwrap() > 32 {
            return false;
        }
        d.remove(&path).unwrap();
        d.sync_dir("wal").unwrap();
        return true;
    }
    false
}

fn corrupt(r: Result<(Vec<u64>, u64, Log<Disk>), LogError>) -> Option<String> {
    match r {
        Err(LogError::Corrupt(why)) => Some(why),
        Err(other) => panic!("a typed refusal other than Corrupt: {other:?}"),
        Ok(_) => None,
    }
}

#[test]
fn an_unsynced_batch_persisted_out_of_order_is_refused_or_truncated_exactly_as_documented() {
    let (mut recovered_whole, mut truncated, mut refused) = (0, 0, 0);
    for seed in 0..600u64 {
        let mut d = disk(seed);
        // Three acknowledged records, then one unsynced batch of three more that spans pages.
        let mut synced = segment_header();
        for lsn in 1..=3 {
            synced.extend_from_slice(&frame(lsn, 20));
        }
        plant(&mut d, SEG1, &synced);
        let batch: Vec<Vec<u8>> = (4..=6).map(|lsn| frame(lsn, 40)).collect();
        for f in &batch {
            d.append(SEG1, f).unwrap();
        }
        crash(&d);

        let after = d.read(SEG1).unwrap();
        assert_eq!(
            after[..synced.len()],
            synced,
            "seed {seed}: a synced byte changed"
        );
        let mut offsets = Vec::new();
        let mut at = synced.len();
        for f in &batch {
            offsets.push(at);
            at += f.len();
        }
        let intact: Vec<bool> = batch
            .iter()
            .zip(&offsets)
            .map(|(f, off)| after.get(*off..*off + f.len()) == Some(f.as_slice()))
            .collect();
        let first_bad = intact.iter().position(|ok| !ok);
        let later_intact = first_bad.is_some_and(|b| intact[b + 1..].iter().any(|ok| *ok));

        let result = open(&d);
        match (first_bad, later_intact) {
            (None, _) => {
                let (seen, cut, _) = result.unwrap();
                assert_eq!(seen, [1, 2, 3, 4, 5, 6], "seed {seed}");
                assert_eq!(cut, 0, "seed {seed}");
                recovered_whole += 1;
            }
            (Some(b), false) => {
                let (seen, cut, _) = result.unwrap();
                let want: Vec<u64> = (1..4 + b as u64).collect();
                assert_eq!(seen, want, "seed {seed}");
                assert_eq!(cut as usize, after.len() - offsets[b], "seed {seed}");
                truncated += 1;
            }
            (Some(b), true) => {
                let why = corrupt(result).unwrap_or_else(|| {
                    panic!("seed {seed}: recovery accepted a damaged region followed by valid data")
                });
                assert!(why.contains("valid records follow"), "seed {seed}: {why}");
                assert_eq!(
                    d.read(SEG1).unwrap(),
                    after,
                    "seed {seed}: refusal changed the file"
                );
                // The documented procedure leaves exactly the records before the damage, so no
                // synced record is lost by following it.
                assert!(operator_repair(&mut d, &why), "seed {seed}: {why}");
                let (seen, _, _) = open(&d).unwrap();
                let want: Vec<u64> = (1..4 + b as u64).collect();
                assert_eq!(seen, want, "seed {seed}");
                refused += 1;
            }
        }
    }
    // All three outcomes are reachable, so the arms above are not vacuous.
    assert!(recovered_whole >= 1, "whole: {recovered_whole}");
    assert!(truncated > 20, "truncated: {truncated}");
    assert!(refused > 20, "refused: {refused}");
}

/// A rotation was cut before the new segment's header reached the disk: the file is the 16 byte
/// header, shortened, zero-filled or partly zero.
#[test]
fn a_rotated_segment_header_lost_to_a_zero_fill_follows_the_documented_policy() {
    let (mut intact, mut short, mut invalid) = (0, 0, 0);
    for seed in 0..600u64 {
        let mut d = disk(seed);
        let mut seg1 = segment_header();
        seg1.extend_from_slice(&frame(1, 20));
        seg1.extend_from_slice(&frame(2, 20));
        plant(&mut d, SEG1, &seg1);
        // rotate(): the header is appended, then fsynced; the cut comes before the fsync.
        let header = segment_header();
        d.append(SEG3, &header).unwrap();
        crash(&d);

        let bytes = d.read(SEG3).unwrap();
        let result = open(&d);
        // The reader validates the magic and the version (the first 12 bytes); the reserved word
        // is not checked, so a header whose last four bytes were torn still reads as valid.
        if bytes.len() >= header.len() && bytes[..12] == header[..12] {
            // Complete header (possibly followed by torn bytes): nothing to repair or refuse.
            let (seen, _, _) = result.unwrap();
            assert_eq!(seen, [1, 2], "seed {seed}");
            intact += 1;
        } else if bytes.len() < header.len() {
            // "A newest segment shorter than its header is a torn creation and is rewritten."
            let (seen, _, log) = result.unwrap();
            assert_eq!(seen, [1, 2], "seed {seed}");
            assert_eq!(log.next_lsn(), 3, "seed {seed}");
            drop(log);
            assert_eq!(
                d.read(SEG3).unwrap(),
                header,
                "seed {seed}: header not rewritten"
            );
            short += 1;
        } else {
            // A complete but invalid header with nothing staged is not provably a creation tear, so
            // recovery refuses and touches nothing. Nothing can be lost: the segment holds no record.
            let why = corrupt(result)
                .unwrap_or_else(|| panic!("seed {seed}: an invalid header {bytes:?} was accepted"));
            assert!(why.contains("header"), "seed {seed}: {why}");
            assert_eq!(
                d.read(SEG3).unwrap(),
                bytes,
                "seed {seed}: refusal changed the file"
            );
            assert!(operator_repair(&mut d, &why), "seed {seed}: {why}");
            let (seen, _, mut log) = open(&d).unwrap();
            assert_eq!(
                seen,
                [1, 2],
                "seed {seed}: the previous segment must be intact"
            );
            assert_eq!(log.append(1, b"after repair").unwrap(), 3, "seed {seed}");
            log.sync().unwrap();
            drop(log);
            let (seen, _, _) = open(&d).unwrap();
            assert_eq!(seen, [1, 2, 3], "seed {seed}");
            invalid += 1;
        }
    }
    assert!(intact > 5, "intact: {intact}");
    assert!(short > 5, "short: {short}");
    assert!(invalid > 5, "invalid: {invalid}");
}

#[test]
fn a_zero_filled_tail_after_acknowledged_records_is_truncated_and_later_writes_survive() {
    for zeros in [1usize, 16, 17, 40, 300] {
        let mut d = disk(1);
        let mut seg = segment_header();
        seg.extend_from_slice(&frame(1, 20));
        seg.extend_from_slice(&frame(2, 20));
        plant(&mut d, SEG1, &seg);
        d.append(SEG1, &vec![0u8; zeros]).unwrap();
        d.sync(SEG1).unwrap();
        crash(&d);

        let (seen, cut, mut log) = open(&d).unwrap();
        assert_eq!(seen, [1, 2], "{zeros} zeros");
        assert_eq!(cut as usize, zeros, "{zeros} zeros");
        assert_eq!(
            d.read(SEG1).unwrap(),
            seg,
            "{zeros} zeros: tail not truncated"
        );
        assert_eq!(log.append(1, b"three").unwrap(), 3);
        log.sync().unwrap();
        drop(log);
        crash(&d);
        let (seen, cut, _) = open(&d).unwrap();
        assert_eq!(
            seen,
            [1, 2, 3],
            "{zeros} zeros: a record written after recovery was lost"
        );
        assert_eq!(cut, 0);
    }
}

/// Witness for the planted bug `ZeroFilledTailKept`: with the defect on, the zeros stay, the next
/// acknowledged record lands behind them, and the next recovery refuses; following the documented
/// procedure (truncate at the reported offset) then discards that acknowledged record. The sim
/// finds the same thing through the `non-prefix` profile.
#[cfg(feature = "planted-bugs")]
#[test]
fn the_planted_zero_tail_bug_strands_an_acknowledged_record_behind_the_zeros() {
    use crate::bugs::{self, Bug};
    let mut d = disk(1);
    let mut seg = segment_header();
    seg.extend_from_slice(&frame(1, 20));
    seg.extend_from_slice(&frame(2, 20));
    plant(&mut d, SEG1, &seg);
    d.append(SEG1, &[0u8; 40]).unwrap();
    d.sync(SEG1).unwrap();
    crash(&d);

    bugs::enable(Bug::ZeroFilledTailKept);
    let opened = open(&d);
    bugs::clear();
    let (seen, cut, mut log) = opened.unwrap();
    assert_eq!(
        (seen, cut),
        (vec![1, 2], 0),
        "the defect must keep the zeros"
    );
    assert_eq!(log.append(1, b"acknowledged").unwrap(), 3);
    log.sync().unwrap();
    drop(log);
    crash(&d);

    let why = corrupt(open(&d)).expect("recovery must now see zeros followed by a valid record");
    assert!(why.contains("valid records follow"), "{why}");
    assert!(operator_repair(&mut d, &why));
    let (seen, _, _) = open(&d).unwrap();
    assert_eq!(
        seen,
        [1, 2],
        "record 3 was acknowledged and is gone: the loss the sim must flag"
    );
}
