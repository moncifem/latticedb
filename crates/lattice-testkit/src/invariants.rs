//! Invariant checkers. Each implements [`Invariant`], runs in the simulator after every recovery,
//! and is the same code an online scrubber would run on a live data directory.

use lattice_core::changeset::{ChangeSet, ChangeSetId, TurnRef, decode_turn_begin};
use lattice_core::record::{Record, RecordType};
use std::collections::{BTreeMap, BTreeSet};

/// One violated invariant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Invariant id, `I1`, `I4`, ... or `RECOVERY` when recovery itself failed.
    pub invariant: &'static str,
    /// Human readable detail.
    pub detail: String,
}

/// The log as recovery returned it, decoded.
#[derive(Debug, Default, Clone)]
pub struct LogView {
    /// LSN of every record, in order.
    pub lsns: Vec<u64>,
    /// Change sets with their LSN.
    pub change_sets: Vec<(u64, ChangeSet)>,
    /// Turn begins with their LSN.
    pub turn_begins: Vec<(u64, TurnRef)>,
    /// Records that failed to decode.
    pub undecodable: usize,
}

impl LogView {
    /// Decodes recovered records.
    pub fn from_records(records: &[Record]) -> Self {
        let mut v = Self::default();
        for r in records {
            v.lsns.push(r.lsn);
            match RecordType::from_u8(r.rtype) {
                Some(RecordType::ChangeSet) => match ChangeSet::decode(&r.payload) {
                    Ok(cs) => v.change_sets.push((r.lsn, cs)),
                    Err(_) => v.undecodable += 1,
                },
                Some(RecordType::TurnBegin) => match decode_turn_begin(&r.payload) {
                    Ok(t) => v.turn_begins.push((r.lsn, t)),
                    Err(_) => v.undecodable += 1,
                },
                Some(RecordType::Checkpoint) => {}
                Some(_) | None => v.undecodable += 1,
            }
        }
        v
    }
}

/// Everything a client submitted for one change set, so the whole envelope can be compared and not
/// just the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expected {
    /// Submitted payload.
    pub payload: Vec<u8>,
    /// Submitted author.
    pub author: u128,
    /// Submitted turn.
    pub turn: Option<TurnRef>,
    /// Submitted parents.
    pub parents: Vec<ChangeSetId>,
}

/// What the test knows must be true, independent of the code under test.
#[derive(Debug, Default, Clone)]
pub struct Oracle {
    /// Every acknowledged change set and what was submitted.
    pub acked: BTreeMap<ChangeSetId, Expected>,
    /// Ids that were rejected and so must not appear in the log.
    pub rejected: Vec<ChangeSetId>,
    /// Acknowledged ids that existed when bit rot last struck. Rot may legitimately destroy them,
    /// so their absence is tolerated. Ids acknowledged **after** the rot are held to the full
    /// guarantee, so a later bug cannot hide behind an earlier fault.
    pub exempt: BTreeSet<ChangeSetId>,
}

/// A checkable property of the log.
pub trait Invariant {
    /// The invariant id this checker reports under.
    fn id(&self) -> &'static str;
    /// Returns violations found in `view` given `oracle`.
    fn check(&self, view: &LogView, oracle: &Oracle) -> Vec<Violation>;
}

fn v(invariant: &'static str, detail: String) -> Violation {
    Violation { invariant, detail }
}

/// I1: every acknowledged commit is present after recovery, with its whole envelope intact.
pub struct AckedDurable;
impl Invariant for AckedDurable {
    fn id(&self) -> &'static str {
        "I1"
    }
    fn check(&self, view: &LogView, oracle: &Oracle) -> Vec<Violation> {
        let present: BTreeMap<_, _> = view.change_sets.iter().map(|(_, c)| (c.id, c)).collect();
        let mut out = Vec::new();
        for (id, want) in &oracle.acked {
            match present.get(id) {
                None if !oracle.exempt.contains(id) => {
                    out.push(v(
                        "I1",
                        format!("acknowledged {id} is missing after recovery"),
                    ));
                }
                Some(c) => {
                    let got = Expected {
                        payload: c.payload.clone(),
                        author: c.author,
                        turn: c.turn,
                        parents: c.parents.clone(),
                    };
                    if &got != want {
                        out.push(v("I1", format!("acknowledged {id} changed on disk")));
                    }
                }
                None => {}
            }
        }
        for id in &oracle.rejected {
            if present.contains_key(id) {
                out.push(v("I1", format!("rejected {id} appears in the log")));
            }
        }
        out
    }
}

/// I2 at log level: records are whole and LSNs are contiguous from 1.
pub struct WholeContiguousRecords;
impl Invariant for WholeContiguousRecords {
    fn id(&self) -> &'static str {
        "I2"
    }
    fn check(&self, view: &LogView, _oracle: &Oracle) -> Vec<Violation> {
        let mut out = Vec::new();
        for (i, lsn) in view.lsns.iter().enumerate() {
            if *lsn != i as u64 + 1 {
                out.push(v(
                    "I2",
                    format!("lsn {lsn} at position {i} breaks contiguity"),
                ));
                break;
            }
        }
        if view.undecodable > 0 {
            out.push(v(
                "I2",
                format!("{} recovered records do not decode", view.undecodable),
            ));
        }
        out
    }
}

/// I4: hybrid logical clock timestamps strictly increase along the log.
pub struct HlcMonotonic;
impl Invariant for HlcMonotonic {
    fn id(&self) -> &'static str {
        "I4"
    }
    fn check(&self, view: &LogView, _oracle: &Oracle) -> Vec<Violation> {
        let mut out = Vec::new();
        let mut prev = None;
        for (lsn, cs) in &view.change_sets {
            if let Some(p) = prev
                && cs.hlc <= p
            {
                out.push(v(
                    "I4",
                    format!("timestamp at lsn {lsn} ({:?}) is not after {p:?}", cs.hlc),
                ));
                break;
            }
            prev = Some(cs.hlc);
        }
        out
    }
}

/// I6: no change set id appears twice.
pub struct NoDuplicateIds;
impl Invariant for NoDuplicateIds {
    fn id(&self) -> &'static str {
        "I6"
    }
    fn check(&self, view: &LogView, _oracle: &Oracle) -> Vec<Violation> {
        let mut seen = BTreeSet::new();
        view.change_sets
            .iter()
            .filter(|(_, c)| !seen.insert(c.id))
            .map(|(lsn, c)| v("I6", format!("{} recorded again at lsn {lsn}", c.id)))
            .collect()
    }
}

/// I12: no write from a turn is recorded after a newer turn of its session began.
pub struct TurnsFenced;
impl Invariant for TurnsFenced {
    fn id(&self) -> &'static str {
        "I12"
    }
    fn check(&self, view: &LogView, _oracle: &Oracle) -> Vec<Violation> {
        let mut out = Vec::new();
        for (lsn, cs) in &view.change_sets {
            let Some(t) = cs.turn else { continue };
            let superseded = view
                .turn_begins
                .iter()
                .any(|(bl, b)| b.session == t.session && b.turn > t.turn && bl < lsn);
            if superseded {
                out.push(v(
                    "I12",
                    format!(
                        "{} from turn {} was recorded after a newer turn began",
                        cs.id, t.turn
                    ),
                ));
            }
        }
        out
    }
}

/// Every checker the simulator runs.
pub fn default_invariants() -> Vec<Box<dyn Invariant>> {
    vec![
        Box::new(AckedDurable),
        Box::new(WholeContiguousRecords),
        Box::new(HlcMonotonic),
        Box::new(NoDuplicateIds),
        Box::new(TurnsFenced),
    ]
}

/// Runs every default checker.
pub fn check_all(view: &LogView, oracle: &Oracle) -> Vec<Violation> {
    default_invariants()
        .iter()
        .flat_map(|i| i.check(view, oracle))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lattice_abi::Hlc;

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

    fn expected(c: &ChangeSet) -> Expected {
        Expected {
            payload: c.payload.clone(),
            author: c.author,
            turn: c.turn,
            parents: c.parents.clone(),
        }
    }

    #[test]
    fn each_checker_flags_what_it_should_and_only_that() {
        let ok = LogView {
            lsns: vec![1, 2],
            change_sets: vec![(1, cs(1, 1, None)), (2, cs(2, 2, None))],
            ..Default::default()
        };
        assert!(check_all(&ok, &Oracle::default()).is_empty());

        let regress = LogView {
            lsns: vec![1, 2],
            change_sets: vec![(1, cs(1, 5, None)), (2, cs(2, 3, None))],
            ..Default::default()
        };
        assert_eq!(check_all(&regress, &Oracle::default())[0].invariant, "I4");

        let dup = LogView {
            lsns: vec![1, 2],
            change_sets: vec![(1, cs(1, 1, None)), (2, cs(1, 2, None))],
            ..Default::default()
        };
        assert_eq!(check_all(&dup, &Oracle::default())[0].invariant, "I6");

        let gap = LogView {
            lsns: vec![1, 3],
            ..Default::default()
        };
        assert_eq!(check_all(&gap, &Oracle::default())[0].invariant, "I2");

        let missing = ChangeSet::new(ChangeSetId::from_raw(9), vec![9]);
        let mut oracle = Oracle::default();
        oracle.acked.insert(missing.id, expected(&missing));
        assert_eq!(check_all(&ok, &oracle)[0].invariant, "I1");
        oracle.exempt.insert(missing.id);
        assert!(
            check_all(&ok, &oracle).is_empty(),
            "rot may eat an id that existed when it struck"
        );
        // An id acknowledged after the rot is not exempt.
        let later = ChangeSet::new(ChangeSetId::from_raw(10), vec![10]);
        oracle.acked.insert(later.id, expected(&later));
        assert_eq!(check_all(&ok, &oracle)[0].invariant, "I1");

        let fenced = LogView {
            lsns: vec![1, 2],
            turn_begins: vec![(
                1,
                TurnRef {
                    session: 1,
                    turn: 2,
                },
            )],
            change_sets: vec![(
                2,
                cs(
                    5,
                    1,
                    Some(TurnRef {
                        session: 1,
                        turn: 1,
                    }),
                ),
            )],
            ..Default::default()
        };
        assert_eq!(check_all(&fenced, &Oracle::default())[0].invariant, "I12");
    }

    #[test]
    fn the_whole_envelope_is_compared_not_just_the_payload() {
        let c = cs(1, 1, None);
        let view = LogView {
            lsns: vec![1],
            change_sets: vec![(1, c.clone())],
            ..Default::default()
        };
        for tamper in [
            |e: &mut Expected| e.author = 99,
            |e: &mut Expected| e.parents = vec![ChangeSetId::from_raw(7)],
            |e: &mut Expected| {
                e.turn = Some(TurnRef {
                    session: 1,
                    turn: 1,
                })
            },
            |e: &mut Expected| e.payload = vec![0, 0],
        ] {
            let mut want = expected(&c);
            tamper(&mut want);
            let mut oracle = Oracle::default();
            oracle.acked.insert(c.id, want);
            assert_eq!(check_all(&view, &oracle)[0].invariant, "I1");
        }
    }
}
