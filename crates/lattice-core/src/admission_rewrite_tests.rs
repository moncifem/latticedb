//! Issue 307: rewriting the guard of an empty tail must never lose a required bit that was already
//! durable, at any crash point of the rewrite.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
#[cfg(feature = "planted-bugs")]
use crate::bugs::{self, Bug};

const SECOND: &str = "wal/00000000000000000002.seg";

/// Gives a test a second capability bit (A = 1, B = 2) before the format defines one.
struct Wide;
impl Wide {
    fn new() -> Self {
        admission::TEST_KNOWN.with(|k| k.set(3));
        Wide
    }
}
impl Drop for Wide {
    fn drop(&mut self) {
        admission::TEST_KNOWN.with(|k| k.set(admission::KNOWN_CAPABILITIES));
    }
}

/// A filesystem that persists a size change at once, before any later append or fsync. SimIo only
/// makes a truncate durable together with the next sync, which hides this ordering.
#[derive(Clone)]
struct EagerTruncate(Disk);
impl Io for EagerTruncate {
    fn exists(&self, p: &str) -> bool {
        self.0.exists(p)
    }
    fn len(&self, p: &str) -> Result<u64, IoError> {
        self.0.len(p)
    }
    fn read(&self, p: &str) -> Result<Vec<u8>, IoError> {
        self.0.read(p)
    }
    fn read_prefix(&self, p: &str, n: usize) -> Result<Vec<u8>, IoError> {
        self.0.read_prefix(p, n)
    }
    fn append(&mut self, p: &str, b: &[u8]) -> Result<(), IoError> {
        self.0.append(p, b)
    }
    fn truncate(&mut self, p: &str, n: u64) -> Result<(), IoError> {
        self.0.truncate(p, n)?;
        self.0.sync(p)
    }
    fn sync(&mut self, p: &str) -> Result<(), IoError> {
        self.0.sync(p)
    }
    fn sync_dir(&mut self, p: &str) -> Result<(), IoError> {
        self.0.sync_dir(p)
    }
    fn list(&self, p: &str) -> Result<Vec<String>, IoError> {
        self.0.list(p)
    }
    fn remove(&mut self, p: &str) -> Result<(), IoError> {
        self.0.remove(p)
    }
    fn create_dir_all(&mut self, p: &str) -> Result<(), IoError> {
        self.0.create_dir_all(p)
    }
    fn lock(&mut self, p: &str) -> Result<(), IoError> {
        self.0.lock(p)
    }
    fn unlock(&mut self, p: &str) {
        self.0.unlock(p)
    }
}

fn open_wide<I: Io>(io: I, seen: &mut Vec<(u64, u64)>) -> Result<Log<I>, LogError> {
    Log::open_streaming_with_capabilities(io, "wal", 1, 3, |r, c| {
        seen.push((r.lsn, c.required_capabilities));
    })
    .map(|(log, _)| log)
}

/// Builds an empty tail (segment 2) whose durable guard is bit A while segment 1 is a legacy
/// segment, power-cuts the rewrite to A|B after `cut` mutating operations, restarts and checks what
/// survived. Returns whether the cut fired, or the first violated property.
fn rewrite_with_cut<I: Io + Clone>(
    disk: &Disk,
    wrap: impl Fn(Disk) -> I,
    cut: u64,
) -> Result<bool, String> {
    let io = wrap(disk.clone());
    let mut seen = Vec::new();
    let mut log = open_wide(io.clone(), &mut seen).map_err(|e| format!("setup open: {e:?}"))?;
    log.append(42, b"one").unwrap();
    log.sync().unwrap();
    // Segment 1 stays legacy; the empty segment 2 gets guard A and the record is never synced.
    log.append_with_capabilities(42, b"two", 1).unwrap();
    drop(log);
    let mut log = open_wide(io.clone(), &mut seen).map_err(|e| format!("setup reopen: {e:?}"))?;
    if log.required_capabilities() != 1 {
        return Err("setup: guard A not durable on the empty tail".into());
    }
    disk.0.borrow_mut().arm_power_cut(cut);
    let result = log.append_with_capabilities(42, b"three", 3);
    let fired = disk.0.borrow().is_powered_off();
    drop(log);
    disk.0.borrow_mut().crash();
    let mut seen = Vec::new();
    let log = open_wide(io.clone(), &mut seen).map_err(|e| format!("recovery refused: {e:?}"))?;
    if seen != vec![(1, 0)] {
        return Err(format!("acknowledged history changed: {seen:?}"));
    }
    if log.required_capabilities() & 1 != 1 {
        return Err(format!(
            "lost durable bit A: {}",
            log.required_capabilities()
        ));
    }
    let header = admission::decode_header(&disk.read(SECOND).unwrap())
        .map_err(|e| format!("segment 2 header: {e:?}"))?;
    if header.required_capabilities & 1 != 1 {
        return Err("segment 2 header lost bit A".into());
    }
    if result.is_ok() && log.required_capabilities() != 3 {
        return Err("an acknowledged install must keep A|B".into());
    }
    if disk
        .list("wal")
        .unwrap()
        .iter()
        .any(|n| n.ends_with(".guard"))
    {
        return Err("staged header left behind after recovery".into());
    }
    drop(log);
    let mut again = Vec::new();
    open_wide(io, &mut again).map_err(|e| format!("second recovery: {e:?}"))?;
    Ok(fired)
}

fn sweep<I: Io + Clone>(wrap: impl Fn(Disk) -> I + Copy) -> Vec<String> {
    let _wide = Wide::new();
    let mut violations = Vec::new();
    for profile in [
        FaultProfile::clean(),
        FaultProfile {
            garbage_tail: false,
            ..FaultProfile::crash_heavy()
        },
        // Torn header appends followed by random bytes: the header may be complete but invalid.
        FaultProfile::crash_heavy(),
    ] {
        for seed in 0..4 {
            let mut cut = 0;
            loop {
                let disk = Disk(
                    Rc::new(RefCell::new(SimIo::new(seed, profile))),
                    Rc::new(Cell::new(0)),
                );
                match rewrite_with_cut(&disk, wrap, cut) {
                    Ok(true) => cut += 1,
                    Ok(false) => break,
                    Err(why) => {
                        violations.push(format!("seed {seed} cut {cut}: {why}"));
                        cut += 1;
                    }
                }
                assert!(cut < 64, "the rewrite never completed");
            }
        }
    }
    violations
}

#[test]
fn no_crash_point_of_an_empty_tail_guard_rewrite_loses_a_durable_bit() {
    for violations in [sweep(|d| d), sweep(EagerTruncate)] {
        assert!(violations.is_empty(), "{violations:#?}");
    }
}

/// Segment 1 legacy with one record, segment 2 an empty tail guarded with bit A.
fn empty_guarded_tail() -> Disk {
    let disk = Disk::new();
    let mut seen = Vec::new();
    let mut log = open_wide(disk.clone(), &mut seen).unwrap();
    log.append(42, b"one").unwrap();
    log.sync().unwrap();
    log.append_with_capabilities(42, b"two", 1).unwrap();
    drop(log);
    disk
}

const STAGE: &str = "wal/00000000000000000002.guard";

/// Replaces the tail with `tail` and optionally leaves a staged header, as a cut rewrite would.
fn torn_rewrite(disk: &Disk, tail: &[u8], stage: Option<Vec<u8>>) {
    let mut io = disk.clone();
    io.truncate(SECOND, 0).unwrap();
    io.append(SECOND, tail).unwrap();
    io.sync(SECOND).unwrap();
    if let Some(stage) = stage {
        io.append(STAGE, &stage).unwrap();
        io.sync(STAGE).unwrap();
    }
    io.sync_dir("wal").unwrap();
}

#[test]
fn a_zero_filled_or_garbage_header_is_repaired_from_a_valid_stage_and_refused_without_one() {
    let _wide = Wide::new();
    let staged = admission::guarded_header(3).unwrap();
    let mut half = staged[..20].to_vec();
    half.extend_from_slice(&[0xAB; 12]);
    for tail in [vec![0u8; 64], vec![0u8; 32], half] {
        let disk = empty_guarded_tail();
        torn_rewrite(&disk, &tail, Some(staged.clone()));
        let mut seen = Vec::new();
        let log = open_wide(disk.clone(), &mut seen).expect("a valid stage repairs the header");
        assert_eq!(log.required_capabilities(), 3);
        assert_eq!(seen, vec![(1, 0)]);
        assert!(
            !disk
                .list("wal")
                .unwrap()
                .iter()
                .any(|n| n.ends_with(".guard"))
        );
        drop(log);
        // No stage: the same bytes are damage and must refuse, without touching the file.
        let disk = empty_guarded_tail();
        torn_rewrite(&disk, &tail, None);
        let before = disk.read(SECOND).unwrap();
        assert!(matches!(
            open_wide(disk.clone(), &mut Vec::new()),
            Err(LogError::Corrupt(_))
        ));
        assert_eq!(disk.read(SECOND).unwrap(), before);
    }
}

#[test]
fn a_stage_that_does_not_decode_refuses_instead_of_being_ignored() {
    let _wide = Wide::new();
    let mut bad = admission::guarded_header(3).unwrap();
    bad[20] ^= 1;
    for (tail, stage) in [
        (vec![0u8; 5], bad.clone()),
        (vec![0u8; 64], bad.clone()),
        (vec![0u8; 5], vec![1, 2, 3]),
    ] {
        let disk = empty_guarded_tail();
        torn_rewrite(&disk, &tail, Some(stage));
        let before = disk.read(SECOND).unwrap();
        assert!(matches!(
            open_wide(disk.clone(), &mut Vec::new()),
            Err(LogError::Corrupt(m)) if m.contains("staged guard header")
        ));
        assert_eq!(disk.read(SECOND).unwrap(), before);
    }
}

#[test]
fn an_invalid_header_over_real_records_is_never_rewritten_from_a_stale_stage() {
    let _wide = Wide::new();
    let disk = empty_guarded_tail();
    let mut log = open_wide(disk.clone(), &mut Vec::new()).unwrap();
    log.append(42, b"three").unwrap();
    log.sync().unwrap();
    drop(log);
    let mut io = disk.clone();
    io.append(STAGE, &admission::guarded_header(3).unwrap())
        .unwrap();
    io.sync(STAGE).unwrap();
    // Corrupt the tail's header only; its record stays.
    let mut bytes = disk.read(SECOND).unwrap();
    bytes[20] ^= 1;
    io.truncate(SECOND, 0).unwrap();
    io.append(SECOND, &bytes).unwrap();
    io.sync(SECOND).unwrap();
    let before = disk.read(SECOND).unwrap();
    assert!(matches!(
        open_wide(disk.clone(), &mut Vec::new()),
        Err(LogError::Corrupt(_))
    ));
    assert_eq!(disk.read(SECOND).unwrap(), before);
}

#[test]
fn recovery_removes_only_files_named_exactly_twenty_digits_and_dot_guard() {
    let _wide = Wide::new();
    let disk = empty_guarded_tail();
    let mut io = disk.clone();
    let keep = [
        "+5.guard",
        "5.guard",
        "0000000000000000000a.guard",
        "00000000000000000002.guard.bak",
        "notes.guard",
    ];
    for name in keep.iter().chain(&["00000000000000000007.guard"]) {
        let path = format!("wal/{name}");
        io.append(&path, b"x").unwrap();
        io.sync(&path).unwrap();
    }
    io.sync_dir("wal").unwrap();
    drop(open_wide(disk.clone(), &mut Vec::new()).unwrap());
    let left = disk.list("wal").unwrap();
    for name in keep {
        assert!(left.iter().any(|n| n == name), "{name} must not be removed");
    }
    assert!(!left.iter().any(|n| n == "00000000000000000007.guard"));
}

#[cfg(feature = "planted-bugs")]
#[test]
fn the_unstaged_in_place_rewrite_is_caught_by_the_sweep() {
    bugs::enable(Bug::GuardRewriteUnstaged);
    let violations = sweep(EagerTruncate);
    bugs::clear();
    assert!(
        violations.iter().any(|v| v.contains("lost durable bit A")),
        "the sweep must catch the old truncate-and-rewrite: {violations:#?}"
    );
}
