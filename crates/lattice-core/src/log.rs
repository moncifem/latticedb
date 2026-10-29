//! The segmented write-ahead log with group commit and crash recovery.
//!
//! Layout: `<dir>/<first-lsn, 20 digits>.seg`, each file a segment header followed by records
//! (see [`crate::record`]). Appends are buffered; [`Log::sync`] writes the buffer and makes it
//! durable with one fsync (group commit). A failed write or fsync **poisons** the log: its
//! durability is unknown, so every later call fails until the process recovers from disk.
//!
//! Only one writer may have a log open: [`Log::open`] takes an exclusive lock that is released when
//! the log is dropped (or the process dies).

use crate::admission::{self, HeaderError, RecoveryContext, SegmentHeader};
use crate::bugs::{self, Bug};
use crate::record::{
    Decoded, MAX_PAYLOAD, RECORD_HEADER_LEN, Record, SEGMENT_HEADER_LEN, decode_record,
    encode_record, segment_header, u32_at, u64_at,
};
use lattice_abi::{Classify, ErrorClass};
use lattice_seams::{Io, IoError};

/// Errors from the log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LogError {
    /// The storage layer failed.
    #[error(transparent)]
    Io(#[from] IoError),
    /// The log on disk is damaged in a way recovery cannot repair.
    #[error("log corrupt: {0}")]
    Corrupt(String),
    /// An earlier write or fsync failed; the log must be reopened.
    #[error("log is poisoned by an earlier failure")]
    Poisoned,
    /// The record is larger than the format allows. Nothing was queued.
    #[error("record of {0} bytes exceeds the {MAX_PAYLOAD} byte limit")]
    TooLarge(usize),
    /// This caller does not understand required engine capabilities. No record was queued.
    #[error("required capabilities {required:#x} exceed supported {supported:#x}")]
    UnsupportedCapabilities {
        /// Features required by the segment or append.
        required: u64,
        /// Features explicitly supported by this consumer.
        supported: u64,
    },
}

impl Classify for LogError {
    fn class(&self) -> ErrorClass {
        match self {
            LogError::Io(e) => e.class(),
            LogError::TooLarge(_) | LogError::UnsupportedCapabilities { .. } => {
                ErrorClass::Rejected
            }
            LogError::Corrupt(_) | LogError::Poisoned => ErrorClass::Fatal,
        }
    }
}

/// What recovery did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Valid records found.
    pub records: u64,
    /// Bytes cut from a torn tail.
    pub truncated_bytes: u64,
    /// Segments scanned.
    pub segments: usize,
}

/// The write-ahead log over an [`Io`].
#[derive(Debug)]
pub struct Log<I: Io> {
    io: I,
    dir: String,
    lock_path: String,
    segment_limit: u64,
    current: String,
    current_len: u64,
    next_lsn: u64,
    durable_lsn: u64,
    buffer: Vec<u8>,
    poisoned: bool,
    supported_capabilities: u64,
    required_capabilities: u64,
    header_len: usize,
}

impl<I: Io> Drop for Log<I> {
    fn drop(&mut self) {
        self.io.unlock(&self.lock_path);
    }
}

fn seg_name(dir: &str, first_lsn: u64) -> String {
    format!("{dir}/{first_lsn:020}.seg")
}

fn first_lsn_of(name: &str) -> Option<u64> {
    name.strip_suffix(".seg")?.parse().ok()
}

fn admit(required: u64, supported: u64) -> Result<(), LogError> {
    if required & !supported != 0 || required & !admission::KNOWN_CAPABILITIES != 0 {
        return Err(LogError::UnsupportedCapabilities {
            required,
            supported,
        });
    }
    Ok(())
}

/// All header admission happens before any recovery callback, repair, truncate or record read.
fn preflight<I: Io>(
    io: &I,
    dir: &str,
    names: &[(u64, String)],
    supported: u64,
) -> Result<Vec<Option<SegmentHeader>>, LogError> {
    let mut previous = 0;
    let mut headers = Vec::with_capacity(names.len());
    for (idx, (_, name)) in names.iter().enumerate() {
        let bytes = io.read_prefix(&format!("{dir}/{name}"), admission::GUARDED_HEADER_LEN)?;
        match admission::decode_header(&bytes) {
            Ok(header) => {
                admit(header.required_capabilities, supported)?;
                if previous & !header.required_capabilities != 0 {
                    return Err(LogError::Corrupt(format!(
                        "segment {name} drops required capabilities"
                    )));
                }
                previous = header.required_capabilities;
                headers.push(Some(header));
            }
            Err(HeaderError::Incomplete) if idx + 1 == names.len() => headers.push(None),
            Err(error) => {
                return Err(LogError::Corrupt(format!(
                    "segment {name} header: {error:?}"
                )));
            }
        }
    }
    Ok(headers)
}

fn header_bytes(required: u64) -> Result<Vec<u8>, LogError> {
    if required == 0 {
        Ok(segment_header())
    } else {
        admission::guarded_header(required)
            .map_err(|e| LogError::Corrupt(format!("guard header: {e:?}")))
    }
}

/// Whether a valid record with an LSN above `min_lsn` starts anywhere after `from`. Used to tell a
/// torn tail (nothing valid follows) from mid-log corruption (valid data follows the damage).
///
/// A candidate must have a plausible LSN (above `min_lsn` and no further ahead than the file could
/// hold records) and a length that fits before its checksum is computed, which bounds the work.
/// The scan is conservative: an opaque payload that happens to embed a valid record makes recovery
/// refuse, never silently lose data (see ADR 0007).
/// `from` is an in-bounds byte offset, including offsets before a segment header would end;
/// this helper scans record bytes and does not interpret or require a segment header.
pub(crate) fn valid_record_after(data: &[u8], from: usize, min_lsn: u64) -> bool {
    let max_gap = (data.len() / RECORD_HEADER_LEN) as u64 + 1;
    let mut off = from + 1;
    while off + RECORD_HEADER_LEN <= data.len() {
        let plausible = match (u32_at(data, off), u64_at(data, off + 8)) {
            (Some(len), Some(lsn)) => {
                lsn > min_lsn
                    && lsn - min_lsn <= max_gap
                    && (len as usize) <= MAX_PAYLOAD
                    && off + RECORD_HEADER_LEN + len as usize <= data.len()
            }
            _ => false,
        };
        if plausible && matches!(decode_record(data, off), Decoded::Record(..)) {
            return true;
        }
        off += 1;
    }
    false
}

/// Creates `dir` and makes every new directory component durable by syncing its parent.
fn ensure_dir<I: Io>(io: &mut I, dir: &str) -> Result<(), IoError> {
    io.create_dir_all(dir)?;
    let mut prefix = String::new();
    for part in dir.split('/').filter(|p| !p.is_empty()) {
        if !bugs::active(Bug::NoParentDirSync) {
            io.sync_dir(&prefix)?;
        }
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
    }
    Ok(())
}

struct Recovered {
    report: RecoveryReport,
    current: String,
    current_len: u64,
    next_lsn: u64,
    durable_lsn: u64,
    required_capabilities: u64,
    header_len: usize,
}

/// Scans the log, repairs a torn tail and hands every valid record to `sink` in LSN order, one at a
/// time, so recovery memory is bounded by one segment rather than by the whole log.
fn recover<I: Io>(
    io: &mut I,
    dir: &str,
    supported: u64,
    sink: &mut dyn FnMut(Record, RecoveryContext),
) -> Result<Recovered, LogError> {
    let mut names: Vec<(u64, String)> = io
        .list(dir)?
        .into_iter()
        .filter_map(|n| first_lsn_of(&n).map(|l| (l, n)))
        .collect();
    names.sort();
    let headers = preflight(io, dir, &names, supported)?;
    let mut required = 0;
    let mut header_len = SEGMENT_HEADER_LEN;
    let mut report = RecoveryReport::default();
    if names.is_empty() {
        let name = seg_name(dir, 1);
        io.append(&name, &segment_header())?;
        io.sync(&name)?;
        io.sync_dir(dir)?;
        return Ok(Recovered {
            report,
            current: name,
            current_len: SEGMENT_HEADER_LEN as u64,
            next_lsn: 1,
            durable_lsn: 0,
            required_capabilities: 0,
            header_len: SEGMENT_HEADER_LEN,
        });
    }
    // One record is held back so the last one can still be dropped by the planted recovery bug.
    let mut pending: Option<(Record, RecoveryContext)> = None;
    let mut count: u64 = 0;
    let mut expected: u64 = 1;
    let mut current = String::new();
    let mut current_len = 0u64;
    let last_idx = names.len() - 1;
    let mut paths = Vec::new();
    for (idx, (first, name)) in names.iter().enumerate() {
        let path = format!("{dir}/{name}");
        paths.push(path.clone());
        let is_last = idx == last_idx;
        if *first != expected {
            return Err(LogError::Corrupt(format!(
                "segment {name} starts at lsn {first}, expected {expected} (missing segment)"
            )));
        }
        let data = io.read(&path)?;
        let header = headers.get(idx).copied().flatten();
        if let Some(header) = header {
            if admission::decode_header(&data) != Ok(header) {
                return Err(LogError::Corrupt(format!(
                    "segment {name} header changed after preflight"
                )));
            }
            required = header.required_capabilities;
            header_len = header.length;
        } else {
            // Only an incomplete newest creation is repaired. Keep prior required bits.
            let bytes = header_bytes(required)?;
            io.truncate(&path, 0)?;
            io.append(&path, &bytes)?;
            io.sync(&path)?;
            report.truncated_bytes += data.len() as u64;
            current = path;
            current_len = bytes.len() as u64;
            header_len = bytes.len();
            report.segments += 1;
            continue;
        }
        let context = RecoveryContext {
            required_capabilities: required,
        };
        let mut off = header_len;
        let mut bad_at: Option<usize> = None;
        while off < data.len() {
            match decode_record(&data, off) {
                Decoded::Record(r, next) if r.lsn == expected => {
                    expected += 1;
                    if let Some(prev) = pending.replace((r, context)) {
                        sink(prev.0, prev.1);
                        count += 1;
                    }
                    off = next;
                }
                _ => {
                    bad_at = Some(off);
                    break;
                }
            }
        }
        if let Some(at) = bad_at {
            if !is_last {
                return Err(LogError::Corrupt(format!(
                    "segment {name} damaged at offset {at} and later segments exist"
                )));
            }
            if valid_record_after(&data, at, expected - 1) {
                return Err(LogError::Corrupt(format!(
                    "segment {name} damaged at offset {at} but valid records follow: refusing to truncate"
                )));
            }
            // A torn tail: drop it.
            if bugs::active(Bug::AcceptTornTail) {
                off = data.len();
            } else {
                io.truncate(&path, at as u64)?;
                io.sync(&path)?;
                report.truncated_bytes += (data.len() - at) as u64;
                off = at;
            }
        }
        current = path;
        current_len = off as u64;
        report.segments += 1;
    }
    if bugs::active(Bug::RecoveryDropsLastRecord)
        && let Some(last) = pending.take()
    {
        let cut = current_len.saturating_sub((RECORD_HEADER_LEN + last.0.payload.len()) as u64);
        io.truncate(&current, cut)?;
        io.sync(&current)?;
        current_len = cut;
        expected -= 1;
    }
    // What recovery returns as committed must actually be durable: bytes that survived a crash, or
    // a failed fsync without a power cut, are visible but not necessarily on disk.
    if !bugs::active(Bug::SkipRecoverySync) {
        for p in &paths {
            io.sync(p)?;
        }
        io.sync_dir(dir)?;
    }
    if let Some(last) = pending.take() {
        sink(last.0, last.1);
        count += 1;
    }
    report.records = count;
    let last_lsn = expected - 1;
    let next_lsn = if bugs::active(Bug::LsnReusedAfterRecovery) && last_lsn > 0 {
        last_lsn
    } else {
        expected
    };
    Ok(Recovered {
        report,
        current,
        current_len,
        next_lsn,
        durable_lsn: last_lsn,
        required_capabilities: required,
        header_len,
    })
}

impl<I: Io> Log<I> {
    /// Opens the log in `dir`, repairs a torn tail and returns recovery counts without collecting
    /// record payloads. Fails if another writer holds the log. Recovery retains segment-name
    /// metadata and one actual segment at a time. Use [`Log::read_from`] for explicit record reads
    /// or [`Log::open_streaming`] to fold records during the recovery pass.
    pub fn open(io: I, dir: &str, segment_limit: u64) -> Result<(Self, RecoveryReport), LogError> {
        Self::open_streaming(io, dir, segment_limit, |_| {})
    }

    /// Like [`Log::open`], but hands each valid record to `visit` in LSN order instead of collecting
    /// them, so recovery memory is bounded by one segment. `visit` also sees records of a log that
    /// then turns out to be damaged beyond repair; in that case the open fails and they must be
    /// discarded.
    pub fn open_streaming(
        io: I,
        dir: &str,
        segment_limit: u64,
        mut visit: impl FnMut(Record),
    ) -> Result<(Self, RecoveryReport), LogError> {
        Self::open_streaming_with_capabilities(io, dir, segment_limit, 0, |record, _| visit(record))
    }

    /// Explicit engine admission; `visit` receives the containing segment's required capabilities.
    /// No callback or repair occurs before the complete header chain passes admission.
    pub fn open_streaming_with_capabilities(
        mut io: I,
        dir: &str,
        segment_limit: u64,
        supported: u64,
        mut visit: impl FnMut(Record, RecoveryContext),
    ) -> Result<(Self, RecoveryReport), LogError> {
        admit(supported, admission::KNOWN_CAPABILITIES)?;
        ensure_dir(&mut io, dir)?;
        let lock_path = format!("{dir}/LOCK");
        io.lock(&lock_path)?;
        match recover(&mut io, dir, supported, &mut visit) {
            Ok(r) => {
                let log = Self {
                    io,
                    dir: dir.to_string(),
                    lock_path,
                    segment_limit,
                    current: r.current,
                    current_len: r.current_len,
                    next_lsn: r.next_lsn,
                    durable_lsn: r.durable_lsn,
                    buffer: Vec::new(),
                    poisoned: false,
                    supported_capabilities: supported,
                    required_capabilities: r.required_capabilities,
                    header_len: r.header_len,
                };
                Ok((log, r.report))
            }
            Err(e) => {
                io.unlock(&lock_path);
                Err(e)
            }
        }
    }

    /// Queues a record and returns its LSN. Nothing is durable until [`Log::sync`]. A record the
    /// format cannot hold is rejected before it takes an LSN.
    pub fn append(&mut self, rtype: u8, payload: &[u8]) -> Result<u64, LogError> {
        self.append_with_capabilities(rtype, payload, 0)
    }

    /// Queues a record only after its required guard is durable. Refusal consumes no LSN.
    pub fn append_with_capabilities(
        &mut self,
        rtype: u8,
        payload: &[u8],
        required: u64,
    ) -> Result<u64, LogError> {
        if self.poisoned {
            return Err(LogError::Poisoned);
        }
        if payload.len() > MAX_PAYLOAD {
            return Err(LogError::TooLarge(payload.len()));
        }
        admit(required, self.supported_capabilities)?;
        if required & !self.required_capabilities != 0 && !bugs::active(Bug::RequiredGuardOmitted) {
            // Flush the legacy prefix before changing segment context; never mix a queued batch.
            self.sync()?;
            if self.poisoned {
                return Err(LogError::Poisoned);
            }
            if let Err(error) = self.install_guard(self.required_capabilities | required) {
                self.poisoned = true;
                return Err(error);
            }
        }
        let lsn = self.next_lsn;
        self.buffer
            .extend_from_slice(&encode_record(lsn, rtype, payload));
        self.next_lsn += 1;
        Ok(lsn)
    }

    /// Writes every queued record and makes them durable with one fsync. Returns the highest
    /// durable LSN. On any failure the log is poisoned and nothing queued may be acknowledged.
    pub fn sync(&mut self) -> Result<u64, LogError> {
        if self.poisoned {
            return Err(LogError::Poisoned);
        }
        if self.buffer.is_empty() {
            return Ok(self.durable_lsn);
        }
        let batch = std::mem::take(&mut self.buffer);
        if let Err(e) = self.io.append(&self.current, &batch) {
            self.poisoned = true;
            return Err(e.into());
        }
        if !bugs::active(Bug::SkipFsync)
            && let Err(e) = self.io.sync(&self.current)
            && !bugs::active(Bug::FsyncErrorSwallowed)
        {
            self.poisoned = !bugs::active(Bug::FsyncFailureNotPoisoning);
            return Err(e.into());
        }
        self.current_len += batch.len() as u64;
        self.durable_lsn = self.next_lsn - 1 + u64::from(bugs::active(Bug::DurableLsnOvershoot));
        if self.current_len >= self.segment_limit && self.rotate().is_err() {
            // The batch is durable and may be acknowledged; the log just cannot take more.
            self.poisoned = true;
        }
        Ok(self.durable_lsn)
    }

    fn rotate(&mut self) -> Result<(), LogError> {
        let name = seg_name(&self.dir, self.next_lsn);
        let bytes = header_bytes(self.required_capabilities)?;
        self.io.append(&name, &bytes)?;
        if !bugs::active(Bug::RotateHeaderNotSynced) {
            self.io.sync(&name)?;
        }
        if !bugs::active(Bug::RotateWithoutDirSync) {
            self.io.sync_dir(&self.dir)?;
        }
        self.current = name;
        self.current_len = bytes.len() as u64;
        self.header_len = bytes.len();
        Ok(())
    }

    fn install_guard(&mut self, required: u64) -> Result<(), LogError> {
        let bytes = header_bytes(required)?;
        let path = if self.current_len == self.header_len as u64 {
            self.io.truncate(&self.current, 0)?;
            self.current.clone()
        } else {
            seg_name(&self.dir, self.next_lsn)
        };
        self.io.append(&path, &bytes)?;
        self.io.sync(&path)?;
        self.io.sync_dir(&self.dir)?;
        self.current = path;
        self.current_len = bytes.len() as u64;
        self.header_len = bytes.len();
        self.required_capabilities = required;
        Ok(())
    }

    /// Engine capabilities explicitly admitted by this log consumer.
    pub fn supported_capabilities(&self) -> u64 {
        self.supported_capabilities
    }

    /// Cumulative required engine capabilities in this log's current segment.
    pub fn required_capabilities(&self) -> u64 {
        self.required_capabilities
    }

    /// Highest LSN known durable.
    pub fn durable_lsn(&self) -> u64 {
        self.durable_lsn
    }

    /// The LSN the next appended record will get.
    pub fn next_lsn(&self) -> u64 {
        self.next_lsn
    }

    /// Whether an earlier failure poisoned the log.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Reads up to `max` durable records starting at `from_lsn`, in LSN order.
    ///
    /// Only records at or below the durable LSN are returned, so a reader never sees a record that
    /// has been queued or written but not yet acknowledged, and never a partial one. Reading does
    /// not touch the writer's state.
    pub fn read_from(&self, from_lsn: u64, max: usize) -> Result<Vec<Record>, LogError> {
        Ok(self
            .read_from_with_context(from_lsn, max)?
            .into_iter()
            .map(|(record, _)| record)
            .collect())
    }

    /// Explicit durable reads retaining required engine capabilities for every record.
    pub fn read_from_with_context(
        &self,
        from_lsn: u64,
        max: usize,
    ) -> Result<Vec<(Record, RecoveryContext)>, LogError> {
        let mut firsts: Vec<u64> = self
            .io
            .list(&self.dir)?
            .into_iter()
            .filter_map(|n| first_lsn_of(&n))
            .collect();
        firsts.sort_unstable();
        let names: Vec<_> = firsts
            .iter()
            .map(|first| (*first, format!("{first:020}.seg")))
            .collect();
        preflight(&self.io, &self.dir, &names, self.supported_capabilities)?;
        // Nothing is ever deleted from the head of the log, so the first segment starts at lsn 1.
        // If everything up to the durable LSN is claimed, the history must start there.
        if self.durable_lsn >= 1 && firsts.first() != Some(&1) {
            return Err(LogError::Corrupt(
                "the first segment is missing: the log does not start at lsn 1".into(),
            ));
        }
        let mut out = Vec::new();
        // The last LSN decoded from any segment scanned so far; everything up to the durable LSN
        // must be reachable, in order, or the log is damaged and the reader must say so.
        let mut reached: Option<u64> = None;
        for (i, first) in firsts.iter().enumerate() {
            let next_first = firsts.get(i + 1).copied().unwrap_or(u64::MAX);
            if next_first <= from_lsn || *first > self.durable_lsn {
                continue;
            }
            let data = self.io.read(&seg_name(&self.dir, *first))?;
            let header = admission::decode_header(&data)
                .map_err(|e| LogError::Corrupt(format!("segment {first} header: {e:?}")))?;
            let mut off = header.length;
            while off < data.len() && out.len() < max {
                let Decoded::Record(r, next) = decode_record(&data, off) else {
                    break;
                };
                if r.lsn > self.durable_lsn {
                    return Ok(out);
                }
                let expected = reached.map_or(*first, |l| l.saturating_add(1));
                if r.lsn != expected {
                    return Err(LogError::Corrupt(format!(
                        "expected lsn {expected} in segment {first}, found {}",
                        r.lsn
                    )));
                }
                reached = Some(r.lsn);
                if r.lsn >= from_lsn {
                    out.push((
                        r,
                        RecoveryContext {
                            required_capabilities: header.required_capabilities,
                        },
                    ));
                }
                off = next;
            }
            if out.len() >= max {
                return Ok(out);
            }
            // The segment ended before the next one starts: a gap or damage below the durable LSN.
            let stop = next_first.saturating_sub(1).min(self.durable_lsn);
            if reached.is_none_or(|l| l < stop) {
                return Err(LogError::Corrupt(format!(
                    "segment {first} ends before lsn {stop}"
                )));
            }
        }
        Ok(out)
    }

    /// The directory this log lives in.
    pub fn dir(&self) -> &str {
        &self.dir
    }

    /// Shared access to the storage, for tests.
    pub fn io(&self) -> &I {
        &self.io
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::encode_record;

    fn log_with(n: u64, limit: u64) -> Log<lattice_sim::SimIo> {
        let io = lattice_sim::SimIo::new(1, lattice_sim::FaultProfile::clean());
        let (mut log, _) = Log::open(io, "wal", limit).unwrap();
        for i in 0..n {
            log.append(1, &[i as u8; 30]).unwrap();
        }
        log
    }

    #[test]
    fn readers_see_only_durable_records_across_segments() {
        let mut log = log_with(6, 120);
        // Nothing is durable yet: the records are only queued.
        assert!(log.read_from(1, 100).unwrap().is_empty());
        log.sync().unwrap();
        let all = log.read_from(1, 100).unwrap();
        assert_eq!(
            all.iter().map(|r| r.lsn).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            log.read_from(4, 2)
                .unwrap()
                .iter()
                .map(|r| r.lsn)
                .collect::<Vec<_>>(),
            [4, 5]
        );
        // A record queued after the sync stays invisible until it is durable.
        log.append(1, b"late").unwrap();
        assert_eq!(log.read_from(7, 10).unwrap().len(), 0);
        log.sync().unwrap();
        assert_eq!(log.read_from(7, 10).unwrap().len(), 1);
        assert!(log.read_from(8, 10).unwrap().is_empty());
    }

    #[test]
    fn an_embedded_record_inside_a_payload_makes_recovery_refuse_rather_than_lose_data() {
        // Documented conservative behaviour (ADR 0007): refusing is safe, truncating would not be.
        let mut data = segment_header();
        data.extend_from_slice(&encode_record(1, 1, b"first"));
        let at = data.len();
        data.extend_from_slice(&[0xFF; 20]);
        assert!(!valid_record_after(&data, at, 1));
        let mut with_valid = data.clone();
        with_valid.extend_from_slice(&encode_record(3, 1, b"later"));
        assert!(valid_record_after(&with_valid, at, 1));
    }

    #[test]
    fn a_valid_record_a_few_lsns_ahead_is_found_whatever_the_data_length() {
        // The plausible-LSN window is the number of records the data could hold. A bug that derives it
        // from a remainder instead shrinks it to one for some lengths, and then valid data after damage
        // looks like a torn tail and gets truncated away.
        let mut found = false;
        for pad in 0..RECORD_HEADER_LEN {
            let mut data = segment_header();
            data.extend_from_slice(&encode_record(1, 1, b"first"));
            let at = data.len();
            data.extend_from_slice(&[0xFF; 20]);
            data.extend_from_slice(&encode_record(4, 1, &vec![7; pad]));
            if data.len().is_multiple_of(RECORD_HEADER_LEN) {
                assert!(valid_record_after(&data, at, 1), "pad {pad}");
                found = true;
            }
        }
        assert!(
            found,
            "no padding made the length a multiple of the header size"
        );
    }

    #[test]
    fn the_scan_ignores_implausible_lsns_without_checksumming() {
        let mut data = segment_header();
        data.extend_from_slice(&[0u8; 400]);
        data.extend_from_slice(&encode_record(1_000_000, 1, b"far"));
        assert!(!valid_record_after(&data, SEGMENT_HEADER_LEN, 1));
    }

    #[test]
    fn a_valid_record_far_beyond_what_the_data_could_hold_is_not_evidence_of_damage() {
        // The window is the number of records the data could hold. A valid record 500 LSNs ahead in a
        // file this small cannot be the continuation of the log, so it must not count.
        let mut data = segment_header();
        data.extend_from_slice(&encode_record(1, 1, b"first"));
        let at = data.len();
        data.extend_from_slice(&[0xFF; 20]);
        data.extend_from_slice(&encode_record(501, 1, b"far"));
        assert!(!valid_record_after(&data, at, 1));
    }

    #[test]
    fn a_read_returns_exactly_as_many_records_as_asked_for() {
        let mut log = log_with(6, 10_000);
        log.sync().unwrap();
        assert_eq!(log.read_from(1, 2).unwrap().len(), 2);
        assert_eq!(log.read_from(1, 1).unwrap().len(), 1);
        assert!(log.read_from(1, 0).unwrap().is_empty());
        assert_eq!(log.read_from(1, 6).unwrap().len(), 6);
        assert_eq!(log.read_from(1, 100).unwrap().len(), 6);
    }

    #[test]
    fn append_accepts_the_payload_limit_and_oversized_refusal_does_not_consume_an_lsn() {
        let mut log = log_with(0, u64::MAX);
        // Share one allocation across both boundary cases; this test checks admission, not timing.
        let payload = vec![0x5A; MAX_PAYLOAD + 1];
        assert_eq!(log.append(1, &payload[..MAX_PAYLOAD]), Ok(1));
        assert_eq!(
            log.append(1, &payload),
            Err(LogError::TooLarge(MAX_PAYLOAD + 1))
        );
        assert_eq!(log.append(1, b"after refusal"), Ok(2));
        assert_eq!(
            log.durable_lsn(),
            0,
            "appending must not acknowledge unsynced data"
        );
    }

    #[test]
    fn recovery_refuses_crc_valid_out_of_order_lsns_without_truncating_later_records() {
        let mut io = lattice_sim::SimIo::new(1, lattice_sim::FaultProfile::clean());
        let mut data = segment_header();
        // Every frame has a valid CRC. The failure is the 1,3,4 sequence, not damaged framing.
        for lsn in [1, 3, 4] {
            data.extend_from_slice(&encode_record(lsn, 1, b"intact"));
        }
        let path = "wal/00000000000000000001.seg";
        io.create_dir_all("wal").unwrap();
        io.append(path, &data).unwrap();
        io.sync(path).unwrap();
        io.sync_dir("wal").unwrap();
        let result = recover(&mut io, "wal", 0, &mut |_, _| {});
        assert!(
            matches!(result, Err(LogError::Corrupt(ref why)) if why.contains("valid records follow"))
        );
        assert_eq!(
            io.read(path).unwrap(),
            data,
            "refusal must preserve every byte"
        );
    }

    #[test]
    fn recovery_damage_scan_starts_strictly_after_the_supplied_offset() {
        let mut data = segment_header();
        let damage = data.len();
        data.push(0xFF);
        let record = data.len();
        data.extend_from_slice(&encode_record(2, 1, b"x"));
        assert!(valid_record_after(&data, damage, 1));
        assert!(
            !valid_record_after(&data, record, 1),
            "a frame at the supplied offset is not a later record"
        );
    }

    #[test]
    fn recovery_damage_scan_requires_an_lsn_strictly_newer_than_the_prefix() {
        let mut data = segment_header();
        let damage = data.len();
        data.push(0xFF);
        data.extend_from_slice(&encode_record(2, 1, b"x"));
        assert!(valid_record_after(&data, damage, 1));
        assert!(
            !valid_record_after(&data, damage, 2),
            "a repeated prefix LSN must not count as continuation"
        );
        assert!(!valid_record_after(&data, damage, 3));
    }

    #[test]
    fn recovery_damage_scan_includes_its_conservative_plausibility_boundary() {
        let mut data = segment_header();
        let damage = data.len();
        data.push(0xFF);
        data.extend_from_slice(&encode_record(3, 1, b""));
        // Sixteen segment-header bytes, one damaged byte and a seventeen-byte frame total 34.
        // The conservative window admits three LSNs ahead (two possible frames plus one allowance).
        assert_eq!(data.len(), 34);
        assert!(valid_record_after(&data, damage, 0));
        let mut beyond = segment_header();
        beyond.push(0xFF);
        beyond.extend_from_slice(&encode_record(4, 1, b""));
        assert!(!valid_record_after(&beyond, damage, 0));
    }

    #[test]
    fn damage_scan_accepts_byte_offsets_without_a_segment_header() {
        for from in 0..16 {
            let mut data = vec![0xFF; from + 1];
            data.extend_from_slice(&encode_record(1, 1, b"intact"));
            assert!(valid_record_after(&data, from, 0), "offset {from}");
            assert!(!valid_record_after(&data, from, 1), "offset {from}");
        }
    }

    #[test]
    fn recovery_preserves_the_immediate_successor_after_a_damaged_byte() {
        let mut io = lattice_sim::SimIo::new(1, lattice_sim::FaultProfile::clean());
        let mut data = segment_header();
        data.extend_from_slice(&encode_record(1, 1, b"first"));
        data.push(0xFF);
        data.extend_from_slice(&encode_record(2, 1, b"second"));
        let path = "wal/00000000000000000001.seg";
        io.create_dir_all("wal").unwrap();
        io.append(path, &data).unwrap();
        io.sync(path).unwrap();
        io.sync_dir("wal").unwrap();
        let result = recover(&mut io, "wal", &mut |_| {});
        assert!(
            matches!(result, Err(LogError::Corrupt(ref why)) if why.contains("valid records follow"))
        );
        assert_eq!(
            io.read(path).unwrap(),
            data,
            "valid LSN 2 must survive refusal"
        );
    }
}
