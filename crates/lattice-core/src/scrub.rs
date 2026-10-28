//! A read-only verifier for a log directory. It shares no code with recovery on purpose: it is the
//! independent check that what recovery would accept is actually consistent, and the same logic
//! backs `lattice doctor` on a data directory and the live scrub of a running kernel.
//!
//! The scrubber never writes, takes no lock, and reports every problem it can find instead of
//! stopping at the first.

use crate::changeset::{
    ChangeSet, ChangeSetId, TurnRef, decode_checkpoint, decode_turn_begin, state_crc,
};
use crate::record::{
    Decoded, HeaderCheck, Record, RecordType, SEGMENT_HEADER_LEN, check_header, decode_record,
};
use lattice_seams::{Io, IoError};
use std::collections::{BTreeMap, BTreeSet};

/// One finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Issue {
    /// A segment starts at an LSN that does not follow the previous segment.
    MissingSegment {
        /// Segment file name.
        segment: String,
        /// The LSN it should have started at.
        expected: u64,
        /// The LSN its name claims.
        found: u64,
    },
    /// The segment header is wrong.
    BadHeader {
        /// Segment file name.
        segment: String,
        /// What is wrong.
        problem: String,
    },
    /// Bytes at the end of the newest segment that do not form a record (a torn tail recovery
    /// would truncate). Not an error by itself, but worth knowing about.
    TornTail {
        /// Segment file name.
        segment: String,
        /// Offset where the damage starts.
        offset: usize,
        /// Bytes after it.
        bytes: usize,
    },
    /// Damage with more log after it: recovery refuses this log.
    MidLogDamage {
        /// Segment file name.
        segment: String,
        /// Offset where the damage starts.
        offset: usize,
    },
    /// A record that is framed correctly but does not decode.
    Undecodable {
        /// Record position.
        lsn: u64,
        /// What failed.
        what: String,
    },
    /// The same change set id appears twice (invariant I6).
    DuplicateId {
        /// The id.
        id: ChangeSetId,
        /// Position of the repeat.
        lsn: u64,
    },
    /// A timestamp is not after the previous one (invariant I4).
    HlcNotIncreasing {
        /// Position of the offending record.
        lsn: u64,
    },
    /// A write from a turn was logged after a newer turn of its session began (invariant I12).
    FencedWriteLogged {
        /// The change set.
        id: ChangeSetId,
        /// Position of the write.
        lsn: u64,
    },
    /// The kernel's in-memory state disagrees with a rebuild from the log (invariant I9).
    DerivedStateMismatch {
        /// What differs.
        what: String,
    },
}

/// What the scrubber found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubReport {
    /// Segments inspected.
    pub segments: usize,
    /// Valid records in order of LSN.
    pub records: usize,
    /// Change sets among them.
    pub change_sets: usize,
    /// Everything wrong, in the order found.
    pub issues: Vec<Issue>,
    /// What a rebuild from the log produces: committed ids, per-session turns.
    pub rebuilt_ids: BTreeSet<ChangeSetId>,
    /// Highest turn per session found in the log.
    pub rebuilt_turns: BTreeMap<u64, u64>,
}

impl ScrubReport {
    /// True when nothing is wrong. A torn tail alone does not count as damage.
    pub fn is_clean(&self) -> bool {
        self.issues
            .iter()
            .all(|i| matches!(i, Issue::TornTail { .. }))
    }
}

fn first_lsn_of(name: &str) -> Option<u64> {
    name.strip_suffix(".seg")?.parse().ok()
}

/// Scans the log in `dir` read-only.
pub fn scrub<I: Io>(io: &I, dir: &str) -> Result<ScrubReport, IoError> {
    let mut report = ScrubReport::default();
    let mut names: Vec<(u64, String)> = io
        .list(dir)?
        .into_iter()
        .filter_map(|n| first_lsn_of(&n).map(|l| (l, n)))
        .collect();
    names.sort();
    let mut expected_lsn = 1u64;
    let mut records: Vec<Record> = Vec::new();
    let last = names.len().saturating_sub(1);
    for (idx, (first, name)) in names.iter().enumerate() {
        report.segments += 1;
        if *first != expected_lsn {
            report.issues.push(Issue::MissingSegment {
                segment: name.clone(),
                expected: expected_lsn,
                found: *first,
            });
            expected_lsn = *first;
        }
        let data = io.read(&format!("{dir}/{name}"))?;
        match check_header(&data) {
            HeaderCheck::Ok => {}
            other => {
                report.issues.push(Issue::BadHeader {
                    segment: name.clone(),
                    problem: format!("{other:?}"),
                });
                continue;
            }
        }
        let mut off = SEGMENT_HEADER_LEN;
        while off < data.len() {
            match decode_record(&data, off) {
                Decoded::Record(r, next) if r.lsn == expected_lsn => {
                    expected_lsn += 1;
                    records.push(r);
                    off = next;
                }
                _ => {
                    // The same bounded, plausibility-filtered scan recovery uses, so a damaged tail full of
                    // look-alike headers cannot make a scrub quadratic.
                    let later =
                        crate::log::valid_record_after(&data, off, expected_lsn.saturating_sub(1));
                    if idx == last && !later {
                        report.issues.push(Issue::TornTail {
                            segment: name.clone(),
                            offset: off,
                            bytes: data.len() - off,
                        });
                    } else {
                        report.issues.push(Issue::MidLogDamage {
                            segment: name.clone(),
                            offset: off,
                        });
                    }
                    break;
                }
            }
        }
    }
    report.records = records.len();
    check_records(&records, &mut report);
    Ok(report)
}

fn check_records(records: &[Record], report: &mut ScrubReport) {
    let mut seen: BTreeSet<ChangeSetId> = BTreeSet::new();
    let mut turns: BTreeMap<u64, u64> = BTreeMap::new();
    let mut last_hlc = None;
    for r in records {
        match RecordType::from_u8(r.rtype) {
            Some(RecordType::ChangeSet) => match ChangeSet::decode(&r.payload) {
                Ok(cs) => {
                    report.change_sets += 1;
                    if !seen.insert(cs.id) {
                        report.issues.push(Issue::DuplicateId {
                            id: cs.id,
                            lsn: r.lsn,
                        });
                    }
                    if last_hlc.is_some_and(|p| cs.hlc <= p) {
                        report.issues.push(Issue::HlcNotIncreasing { lsn: r.lsn });
                    }
                    last_hlc = Some(cs.hlc);
                    if let Some(TurnRef { session, turn }) = cs.turn
                        && turns.get(&session) != Some(&turn)
                    {
                        report.issues.push(Issue::FencedWriteLogged {
                            id: cs.id,
                            lsn: r.lsn,
                        });
                    }
                }
                Err(e) => report.issues.push(Issue::Undecodable {
                    lsn: r.lsn,
                    what: format!("change set: {e}"),
                }),
            },
            Some(RecordType::TurnBegin) => match decode_turn_begin(&r.payload) {
                Ok(t) => {
                    let cur = turns.entry(t.session).or_insert(0);
                    *cur = (*cur).max(t.turn);
                }
                Err(e) => report.issues.push(Issue::Undecodable {
                    lsn: r.lsn,
                    what: format!("turn begin: {e}"),
                }),
            },
            Some(RecordType::Checkpoint) => match decode_checkpoint(&r.payload) {
                Ok(c) => {
                    if c.through_lsn.checked_add(1) != Some(r.lsn)
                        || c.change_sets != seen.len() as u64
                        || c.state_crc != state_crc(&seen, &turns)
                    {
                        report.issues.push(Issue::DerivedStateMismatch {
                            what: format!("checkpoint at lsn {} disagrees with the log", r.lsn),
                        });
                    }
                }
                Err(e) => report.issues.push(Issue::Undecodable {
                    lsn: r.lsn,
                    what: format!("checkpoint: {e}"),
                }),
            },
            _ => {}
        }
    }
    report.rebuilt_ids = seen;
    report.rebuilt_turns = turns;
}
