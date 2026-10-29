//! Sequence CRDT for text (P3-07, E3-6; invariant I5).
//!
//! An operation-based replicated growable array (RGA) with stable element ids. Every character has
//! an [`ElemId`]: the hybrid logical clock id of the insert operation that created it plus its
//! offset inside that operation's chunk. An insert names the element it goes *after* (or the head)
//! and carries a chunk of up to [`MAX_CHUNK_CHARS`] characters; a delete names one element.
//!
//! Ordering rule (the whole algorithm): the document is the depth-first order of the "inserted
//! after" tree in which siblings are ordered newest first by `(op id, offset)`. It is a function of
//! the *set* of operations, so any delivery order converges:
//!
//! - Duplicates are ignored (insert ids are remembered; a delete only lowers the element's delete
//!   stamp, so repeating it changes nothing).
//! - An operation whose target element has not arrived yet is parked (at most [`MAX_PENDING`])
//!   and applied as soon as the target exists.
//! - An operation must be causal: its id is greater than the id of the operation it refers to.
//!   Hybrid logical clocks that observe received ids guarantee this; others are rejected.
//! - Deleting never removes an element. It records the *smallest* delete id seen, so concurrent
//!   deletes agree, and "visible at point p" is `insert <= p < delete`.
//!
//! # Compaction and revert points
//!
//! Tombstones are kept so that a revert point (an HLC the caller declares worth restoring) can show
//! the text as of that point with [`Doc::text_at`]. [`Doc::compact`] drops a tombstone only when its
//! delete id is at or below the caller's *stable* frontier and no retained point falls in its visible
//! interval `[insert, delete)`. Everything inserted or deleted after a retained point therefore
//! stays. Compaction is local and is only safe when every replica has seen every operation at or
//! below `stable`, and nobody will insert after, or delete, an element that is already gone; a late
//! reference to a dropped element is reported as [`TextError::AnchorGone`] (an already compacted
//! delete is a harmless duplicate).
//!
//! # Operation encoding (version 1, little endian, canonical)
//!
//! Insert: `version=1`, `kind=1`, id (`Hlc::to_bytes`, 16), `flag` (0 = after head, 1 = after an
//! element), then for flag 1 the anchor operation id (16) and offset (u32), then the text length in
//! bytes (u16) and the UTF-8 text. Delete: `version=1`, `kind=2`, id (16), target operation id
//! (16), target offset (u32), 38 bytes in all. Decoding rejects truncation, trailing bytes, unknown
//! version, kind or flag, invalid UTF-8, empty or over-long text, an all-zero id, an offset beyond
//! the chunk limit and a non-causal reference, so the accepted encodings are exactly the canonical
//! ones: `decode(b)` succeeding implies `encode() == b`.

use lattice_abi::Hlc;
use std::collections::BTreeSet;

/// Most characters one insert may carry.
pub const MAX_CHUNK_CHARS: usize = 256;
/// Most operations parked while waiting for their target.
pub const MAX_PENDING: usize = 4096;
/// Longest encoded insert: header, anchor, length prefix and four bytes per character.
pub const MAX_ENCODED_LEN: usize = 2 + 16 + 1 + 20 + 2 + MAX_CHUNK_CHARS * 4;

const VERSION: u8 = 1;
const KIND_INSERT: u8 = 1;
const KIND_DELETE: u8 = 2;
const ZERO: Hlc = Hlc {
    wall_ms: 0,
    counter: 0,
    node: 0,
};

/// Stable identity of one character: its insert operation and its offset in that chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ElemId {
    /// Id of the insert operation that created the character.
    pub op: Hlc,
    /// Position inside that operation's chunk.
    pub off: u32,
}

/// One replicated text operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Insert `text` after `after` (`None` is the head of the document).
    Insert {
        /// Operation id; also the id of the first character.
        id: Hlc,
        /// The element to insert after.
        after: Option<ElemId>,
        /// The chunk, one to [`MAX_CHUNK_CHARS`] characters.
        text: String,
    },
    /// Delete one element.
    Delete {
        /// Operation id, which orders the delete against revert points.
        id: Hlc,
        /// The element to delete.
        target: ElemId,
    },
}

/// Why an operation is malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OpError {
    /// The operation id is all zero, which marks "not deleted" internally.
    #[error("operation id must not be zero")]
    ZeroId,
    /// An insert with no characters.
    #[error("insert text is empty")]
    EmptyText,
    /// An insert with more than [`MAX_CHUNK_CHARS`] characters.
    #[error("insert text longer than {MAX_CHUNK_CHARS} characters")]
    ChunkTooLong,
    /// A referenced offset cannot exist in any chunk.
    #[error("element offset out of range")]
    BadOffset,
    /// The operation id is not greater than the id of the operation it refers to.
    #[error("operation does not causally follow its target")]
    NotCausal,
}

/// Why an operation could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TextDecodeError {
    /// The input ended early.
    #[error("truncated text op")]
    Truncated,
    /// Bytes remain after the operation.
    #[error("trailing bytes after text op")]
    TrailingBytes,
    /// Unknown encoding version.
    #[error("unknown text op version {0}")]
    BadVersion(u8),
    /// Unknown operation kind.
    #[error("unknown text op kind {0}")]
    BadKind(u8),
    /// The anchor flag is neither 0 nor 1.
    #[error("bad anchor flag {0}")]
    BadFlag(u8),
    /// The text is not valid UTF-8.
    #[error("text is not valid UTF-8")]
    InvalidUtf8,
    /// Structurally decoded but not a valid operation.
    #[error("invalid text op: {0}")]
    Invalid(OpError),
}

/// Why [`Doc::apply`] refused an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TextError {
    /// The operation is malformed.
    #[error("{0}")]
    Op(OpError),
    /// An insert id was parked twice with different content.
    #[error("operation id reused with different content")]
    ConflictingOpId,
    /// Too many operations are waiting for their target.
    #[error("too many pending operations")]
    PendingFull,
    /// The referenced element was compacted away or never existed in its chunk.
    #[error("referenced element is gone")]
    AnchorGone,
}

/// What [`Doc::apply`] did with an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The operation took effect.
    Applied,
    /// Nothing changed: already applied or already compacted.
    Duplicate,
    /// The target has not arrived; the operation is parked.
    Pending,
}

impl Op {
    /// The operation id.
    pub fn id(&self) -> Hlc {
        match self {
            Op::Insert { id, .. } | Op::Delete { id, .. } => *id,
        }
    }

    /// The operation this one refers to, if any.
    fn target_op(&self) -> Option<Hlc> {
        match self {
            Op::Insert { after, .. } => after.map(|a| a.op),
            Op::Delete { target, .. } => Some(target.op),
        }
    }

    /// Checks the rules that do not depend on any replica state.
    pub fn validate(&self) -> Result<(), OpError> {
        if self.id() == ZERO {
            return Err(OpError::ZeroId);
        }
        if let Op::Insert { text, .. } = self {
            let n = text.chars().count();
            if n == 0 {
                return Err(OpError::EmptyText);
            }
            if n > MAX_CHUNK_CHARS {
                return Err(OpError::ChunkTooLong);
            }
        }
        let reference = match self {
            Op::Insert { after, .. } => *after,
            Op::Delete { target, .. } => Some(*target),
        };
        if let Some(r) = reference {
            if r.off as usize >= MAX_CHUNK_CHARS {
                return Err(OpError::BadOffset);
            }
            if self.target_op().is_some_and(|t| t >= self.id()) {
                return Err(OpError::NotCausal);
            }
        }
        Ok(())
    }

    /// Encodes as documented in the module header.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        match self {
            Op::Insert { id, after, text } => {
                out.push(KIND_INSERT);
                out.extend_from_slice(&id.to_bytes());
                match after {
                    None => out.push(0),
                    Some(a) => {
                        out.push(1);
                        out.extend_from_slice(&a.op.to_bytes());
                        out.extend_from_slice(&a.off.to_le_bytes());
                    }
                }
                let len = u16::try_from(text.len()).unwrap_or(u16::MAX);
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(text.as_bytes());
            }
            Op::Delete { id, target } => {
                out.push(KIND_DELETE);
                out.extend_from_slice(&id.to_bytes());
                out.extend_from_slice(&target.op.to_bytes());
                out.extend_from_slice(&target.off.to_le_bytes());
            }
        }
        out
    }

    /// Decodes strictly and with bounded work: never allocates more than the input length.
    pub fn decode(bytes: &[u8]) -> Result<Self, TextDecodeError> {
        let mut r = Reader { rest: bytes };
        let version = r.u8()?;
        if version != VERSION {
            return Err(TextDecodeError::BadVersion(version));
        }
        let kind = r.u8()?;
        let id = r.hlc()?;
        let op = match kind {
            KIND_INSERT => {
                let after = match r.u8()? {
                    0 => None,
                    1 => Some(ElemId {
                        op: r.hlc()?,
                        off: r.u32()?,
                    }),
                    f => return Err(TextDecodeError::BadFlag(f)),
                };
                let len = usize::from(r.u16()?);
                let text = std::str::from_utf8(r.take(len)?)
                    .map_err(|_| TextDecodeError::InvalidUtf8)?
                    .to_owned();
                Op::Insert { id, after, text }
            }
            KIND_DELETE => Op::Delete {
                id,
                target: ElemId {
                    op: r.hlc()?,
                    off: r.u32()?,
                },
            },
            k => return Err(TextDecodeError::BadKind(k)),
        };
        if !r.rest.is_empty() {
            return Err(TextDecodeError::TrailingBytes);
        }
        op.validate().map_err(TextDecodeError::Invalid)?;
        Ok(op)
    }
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], TextDecodeError> {
        if self.rest.len() < n {
            return Err(TextDecodeError::Truncated);
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, TextDecodeError> {
        self.take(1)?
            .first()
            .copied()
            .ok_or(TextDecodeError::Truncated)
    }

    fn u16(&mut self) -> Result<u16, TextDecodeError> {
        let b: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| TextDecodeError::Truncated)?;
        Ok(u16::from_le_bytes(b))
    }

    fn u32(&mut self) -> Result<u32, TextDecodeError> {
        let b: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| TextDecodeError::Truncated)?;
        Ok(u32::from_le_bytes(b))
    }

    fn hlc(&mut self) -> Result<Hlc, TextDecodeError> {
        let b: [u8; Hlc::ENCODED_LEN] = self
            .take(Hlc::ENCODED_LEN)?
            .try_into()
            .map_err(|_| TextDecodeError::Truncated)?;
        Ok(Hlc::from_bytes(&b))
    }
}

/// One stored character.
#[derive(Debug, Clone, Copy)]
struct Elem {
    id: ElemId,
    ch: char,
    /// Smallest delete id seen; [`ZERO`] while the character is alive.
    deleted: Hlc,
}

impl Elem {
    fn visible_at(&self, point: Hlc) -> bool {
        self.id.op <= point && (self.deleted == ZERO || self.deleted > point)
    }
}

/// What [`Doc::compact`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compaction {
    /// Tombstones dropped.
    pub removed: usize,
    /// Tombstones still stored.
    pub tombstones_left: usize,
}

/// A replica of one text document.
#[derive(Debug, Clone, Default)]
pub struct Doc {
    elems: Vec<Elem>,
    /// Ids of every insert ever applied, kept across compaction so duplicates stay duplicates.
    applied: BTreeSet<Hlc>,
    pending: Vec<Op>,
}

impl Doc {
    /// An empty document.
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies an operation in any delivery order, any number of times.
    pub fn apply(&mut self, op: Op) -> Result<Outcome, TextError> {
        op.validate().map_err(TextError::Op)?;
        let outcome = self.try_apply(&op)?;
        if outcome == Outcome::Pending {
            if let Some(same) = self.pending.iter().find(|p| p.id() == op.id()) {
                return if *same == op {
                    Ok(Outcome::Pending)
                } else {
                    Err(TextError::ConflictingOpId)
                };
            }
            if self.pending.len() >= MAX_PENDING {
                return Err(TextError::PendingFull);
            }
            self.pending.push(op);
            return Ok(Outcome::Pending);
        }
        if outcome == Outcome::Applied {
            self.drain_pending();
        }
        Ok(outcome)
    }

    fn drain_pending(&mut self) {
        loop {
            let mut progressed = false;
            let parked = std::mem::take(&mut self.pending);
            for op in parked {
                match self.try_apply(&op) {
                    Ok(Outcome::Pending) => self.pending.push(op),
                    Ok(Outcome::Applied) => progressed = true,
                    // A parked operation that turns out unusable is dropped, never retried.
                    Ok(Outcome::Duplicate) | Err(_) => {}
                }
            }
            if !progressed {
                return;
            }
        }
    }

    fn index_of(&self, id: ElemId) -> Option<usize> {
        self.elems.iter().position(|e| e.id == id)
    }

    fn try_apply(&mut self, op: &Op) -> Result<Outcome, TextError> {
        match op {
            Op::Insert { id, after, text } => {
                if self.applied.contains(id) {
                    return Ok(Outcome::Duplicate);
                }
                let mut at = match after {
                    None => 0,
                    Some(a) => {
                        if !self.applied.contains(&a.op) {
                            return Ok(Outcome::Pending);
                        }
                        self.index_of(*a).ok_or(TextError::AnchorGone)? + 1
                    }
                };
                // Skip newer siblings and their subtrees: every such element is newer than us.
                while self.elems.get(at).is_some_and(|e| e.id.op > *id) {
                    at += 1;
                }
                let chunk = text.chars().zip(0u32..).map(|(ch, off)| Elem {
                    id: ElemId { op: *id, off },
                    ch,
                    deleted: ZERO,
                });
                self.elems.splice(at..at, chunk);
                self.applied.insert(*id);
                Ok(Outcome::Applied)
            }
            Op::Delete { id, target } => {
                if !self.applied.contains(&target.op) {
                    return Ok(Outcome::Pending);
                }
                let Some(i) = self.index_of(*target) else {
                    // Compacted: it was deleted long ago, so this is a late duplicate.
                    return Ok(Outcome::Duplicate);
                };
                let Some(e) = self.elems.get_mut(i) else {
                    return Ok(Outcome::Duplicate);
                };
                if e.deleted != ZERO && e.deleted <= *id {
                    return Ok(Outcome::Duplicate);
                }
                e.deleted = *id;
                Ok(Outcome::Applied)
            }
        }
    }

    /// The current visible text.
    pub fn text(&self) -> String {
        self.elems
            .iter()
            .filter(|e| e.deleted == ZERO)
            .map(|e| e.ch)
            .collect()
    }

    /// The text as of revert point `point`: inserts at or before it, minus deletes at or before it.
    /// Exact for any point while history is whole, and for every retained point after compaction.
    pub fn text_at(&self, point: Hlc) -> String {
        self.elems
            .iter()
            .filter(|e| e.visible_at(point))
            .map(|e| e.ch)
            .collect()
    }

    /// Id of the `index`-th visible character, for choosing an insert anchor or a delete target.
    pub fn visible_id(&self, index: usize) -> Option<ElemId> {
        self.elems
            .iter()
            .filter(|e| e.deleted == ZERO)
            .nth(index)
            .map(|e| e.id)
    }

    /// Visible characters.
    pub fn len(&self) -> usize {
        self.elems.iter().filter(|e| e.deleted == ZERO).count()
    }

    /// True when no character is visible.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stored tombstones.
    pub fn tombstones(&self) -> usize {
        self.elems.iter().filter(|e| e.deleted != ZERO).count()
    }

    /// Stored elements, visible and deleted.
    pub fn stored(&self) -> usize {
        self.elems.len()
    }

    /// Operations waiting for their target.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Bytes of one stored element.
    pub const ELEM_BYTES: usize = std::mem::size_of::<Elem>();

    /// Heap bytes held by the element array (capacity, not length).
    pub fn element_heap_bytes(&self) -> usize {
        self.elems.capacity() * Self::ELEM_BYTES
    }

    /// Drops tombstones that no retained revert point can show and that every replica has seen
    /// deleted (`delete id <= stable`). See the module header for the safety contract.
    pub fn compact(&mut self, stable: Hlc, retained: &[Hlc]) -> Compaction {
        let before = self.elems.len();
        self.elems.retain(|e| {
            e.deleted == ZERO
                || e.deleted > stable
                || retained.iter().any(|p| e.id.op <= *p && *p < e.deleted)
        });
        self.elems.shrink_to_fit();
        Compaction {
            removed: before - self.elems.len(),
            tombstones_left: self.tombstones(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    fn h(wall: u64, node: u32) -> Hlc {
        Hlc {
            wall_ms: wall,
            counter: 0,
            node,
        }
    }

    fn ins(id: Hlc, after: Option<ElemId>, text: &str) -> Op {
        Op::Insert {
            id,
            after,
            text: text.to_owned(),
        }
    }

    fn el(op: Hlc, off: u32) -> ElemId {
        ElemId { op, off }
    }

    /// Independent oracle: depth-first order of the after-tree, newest sibling first, built from
    /// the operation set alone. Only operations stamped at or before `point` count.
    fn oracle(ops: &[Op], point: Hlc, with_deletes: bool) -> String {
        let mut kids: BTreeMap<Option<ElemId>, Vec<(ElemId, char)>> = BTreeMap::new();
        let mut dead: BTreeSet<ElemId> = BTreeSet::new();
        for op in ops.iter().filter(|o| o.id() <= point) {
            match op {
                Op::Insert { id, after, text } => {
                    let mut parent = *after;
                    for (ch, off) in text.chars().zip(0u32..) {
                        let me = el(*id, off);
                        kids.entry(parent).or_default().push((me, ch));
                        parent = Some(me);
                    }
                }
                Op::Delete { target, .. } => {
                    if with_deletes {
                        dead.insert(*target);
                    }
                }
            }
        }
        for v in kids.values_mut() {
            v.sort_by_key(|a| std::cmp::Reverse(a.0));
        }
        let mut out = String::new();
        let mut stack: Vec<(ElemId, char)> = kids
            .get(&None)
            .map(|v| v.iter().rev().copied().collect())
            .unwrap_or_default();
        while let Some((id, ch)) = stack.pop() {
            if !dead.contains(&id) {
                out.push(ch);
            }
            if let Some(v) = kids.get(&Some(id)) {
                stack.extend(v.iter().rev().copied());
            }
        }
        out
    }

    fn build(ops: &[Op]) -> Doc {
        let mut d = Doc::new();
        for op in ops {
            d.apply(op.clone()).unwrap();
        }
        d
    }

    #[test]
    fn typing_and_deleting() {
        let a = h(1, 1);
        let mut d = Doc::new();
        d.apply(ins(a, None, "hllo")).unwrap();
        d.apply(ins(h(2, 1), Some(el(a, 0)), "e")).unwrap();
        assert_eq!(d.text(), "hello");
        d.apply(Op::Delete {
            id: h(3, 1),
            target: el(a, 0),
        })
        .unwrap();
        assert_eq!(d.text(), "ello");
        assert_eq!(d.text_at(h(2, 1)), "hello");
        assert_eq!(d.text_at(h(0, 9)), "");
    }

    #[test]
    fn concurrent_inserts_at_one_spot_do_not_interleave() {
        let base = ins(h(1, 1), None, "ab");
        let x = ins(h(2, 1), Some(el(h(1, 1), 0)), "XX");
        let y = ins(h(2, 2), Some(el(h(1, 1), 0)), "YY");
        let one = build(&[base.clone(), x.clone(), y.clone()]);
        let two = build(&[base, y, x]);
        assert_eq!(one.text(), two.text());
        assert_eq!(one.text(), "aYYXXb");
    }

    #[test]
    fn duplicate_pending_and_conflict() {
        let a = ins(h(1, 1), None, "a");
        let b = ins(h(2, 1), Some(el(h(1, 1), 0)), "b");
        let mut d = Doc::new();
        assert_eq!(d.apply(b.clone()).unwrap(), Outcome::Pending);
        assert_eq!(d.apply(b.clone()).unwrap(), Outcome::Pending);
        assert_eq!(d.pending(), 1);
        let clash = ins(h(2, 1), Some(el(h(1, 1), 0)), "z");
        assert_eq!(d.apply(clash), Err(TextError::ConflictingOpId));
        assert_eq!(d.apply(a.clone()).unwrap(), Outcome::Applied);
        assert_eq!(d.pending(), 0);
        assert_eq!(d.text(), "ab");
        assert_eq!(d.apply(a).unwrap(), Outcome::Duplicate);
        assert_eq!(d.apply(b).unwrap(), Outcome::Duplicate);
        assert_eq!(d.text(), "ab");
    }

    #[test]
    fn rejects_malformed_ops() {
        let z = Op::Insert {
            id: ZERO,
            after: None,
            text: "a".into(),
        };
        assert_eq!(z.validate(), Err(OpError::ZeroId));
        assert_eq!(ins(h(1, 1), None, "").validate(), Err(OpError::EmptyText));
        let long = "x".repeat(MAX_CHUNK_CHARS + 1);
        assert_eq!(
            ins(h(1, 1), None, &long).validate(),
            Err(OpError::ChunkTooLong)
        );
        assert_eq!(
            ins(h(1, 1), Some(el(h(1, 1), 0)), "a").validate(),
            Err(OpError::NotCausal)
        );
        assert_eq!(
            ins(h(2, 1), Some(el(h(1, 1), 256)), "a").validate(),
            Err(OpError::BadOffset)
        );
    }

    #[test]
    fn compaction_keeps_revert_points() {
        let a = h(1, 1);
        let mut d = Doc::new();
        d.apply(ins(a, None, "abc")).unwrap();
        d.apply(Op::Delete {
            id: h(5, 1),
            target: el(a, 1),
        })
        .unwrap();
        d.apply(Op::Delete {
            id: h(9, 1),
            target: el(a, 2),
        })
        .unwrap();
        // 'b' was visible during [1, 5); 'c' during [1, 9).
        let c = d.compact(h(20, 0), &[h(3, 0)]);
        assert_eq!(c.removed, 0);
        let c = d.compact(h(20, 0), &[h(6, 0)]);
        assert_eq!(c.removed, 1);
        assert_eq!(d.text_at(h(6, 0)), "ac");
        assert_eq!(d.text(), "a");
        let c = d.compact(h(20, 0), &[]);
        assert_eq!(c.removed, 1);
        assert_eq!(c.tombstones_left, 0);
        assert_eq!(d.text(), "a");
        // A late duplicate of a compacted delete is harmless.
        let late = Op::Delete {
            id: h(9, 1),
            target: el(a, 2),
        };
        assert_eq!(d.apply(late).unwrap(), Outcome::Duplicate);
        // A late insert after a compacted element is reported.
        let bad = ins(h(30, 1), Some(el(a, 2)), "z");
        assert_eq!(d.apply(bad), Err(TextError::AnchorGone));
    }

    #[test]
    fn compaction_respects_the_stable_frontier() {
        let a = h(1, 1);
        let mut d = Doc::new();
        d.apply(ins(a, None, "ab")).unwrap();
        d.apply(Op::Delete {
            id: h(8, 1),
            target: el(a, 0),
        })
        .unwrap();
        assert_eq!(d.compact(h(7, 0), &[]).removed, 0);
        assert_eq!(d.compact(h(8, 1), &[]).removed, 1);
    }

    #[test]
    fn encoding_is_exact_and_strict() {
        let i = ins(h(2, 1), Some(el(h(1, 1), 3)), "héllo");
        let bytes = i.encode();
        assert_eq!(Op::decode(&bytes).unwrap(), i);
        assert_eq!(bytes.len(), 2 + 16 + 1 + 20 + 2 + "héllo".len());
        let del = Op::Delete {
            id: h(3, 1),
            target: el(h(2, 1), 0),
        };
        let db = del.encode();
        assert_eq!(db.len(), 38);
        assert_eq!(Op::decode(&db).unwrap(), del);
        let mut extra = db.clone();
        extra.push(0);
        assert_eq!(Op::decode(&extra), Err(TextDecodeError::TrailingBytes));
        assert_eq!(Op::decode(&db[..37]), Err(TextDecodeError::Truncated));
        let mut v = db.clone();
        v[0] = 2;
        assert_eq!(Op::decode(&v), Err(TextDecodeError::BadVersion(2)));
        let mut k = db.clone();
        k[1] = 9;
        assert_eq!(Op::decode(&k), Err(TextDecodeError::BadKind(9)));
        let mut f = bytes.clone();
        f[18] = 2;
        assert_eq!(Op::decode(&f), Err(TextDecodeError::BadFlag(2)));
        let bad_utf8 = ins(h(2, 1), None, "a");
        let mut u = bad_utf8.encode();
        let last = u.len() - 1;
        u[last] = 0xff;
        assert_eq!(Op::decode(&u), Err(TextDecodeError::InvalidUtf8));
    }

    // ----- generated histories ------------------------------------------------------------

    /// One scripted editing step: which replica acts, what it does, who it hears from first.
    #[derive(Debug, Clone)]
    struct Step {
        replica: usize,
        kind: u8,
        pos: usize,
        text: u8,
        sync_from: usize,
    }

    fn step_strategy() -> impl Strategy<Value = Step> {
        (0usize..3, 0u8..10, 0usize..64, 0u8..60, 0usize..3).prop_map(
            |(replica, kind, pos, text, sync_from)| Step {
                replica,
                kind,
                pos,
                text,
                sync_from,
            },
        )
    }

    /// Runs the script on three replicas that sometimes sync, and returns every operation
    /// issued. Ids come from a per-replica Lamport clock that observes what the replica has seen,
    /// so operations are causal exactly as hybrid logical clocks would make them.
    fn run_script(steps: &[Step]) -> Vec<Op> {
        let mut docs = [Doc::new(), Doc::new(), Doc::new()];
        let mut seen: [Vec<Op>; 3] = [vec![], vec![], vec![]];
        let mut clock = [0u64; 3];
        let mut all: Vec<Op> = vec![];
        for s in steps {
            let r = s.replica;
            let from = s.sync_from;
            if from != r {
                let incoming: Vec<Op> = seen[from].clone();
                for op in incoming {
                    clock[r] = clock[r].max(op.id().wall_ms);
                    docs[r].apply(op.clone()).unwrap();
                    if !seen[r].contains(&op) {
                        seen[r].push(op);
                    }
                }
            }
            clock[r] += 1;
            let id = h(clock[r], u32::try_from(r).unwrap() + 1);
            let len = docs[r].len();
            let op = if s.kind < 7 || len == 0 {
                let after = if len == 0 || s.pos % (len + 1) == 0 {
                    None
                } else {
                    docs[r].visible_id(s.pos % (len + 1) - 1)
                };
                let n = usize::from(s.text % 3) + 1;
                let text: String = (0..n)
                    .map(|i| char::from(b'a' + (s.text + u8::try_from(i).unwrap()) % 26))
                    .collect();
                Op::Insert { id, after, text }
            } else {
                Op::Delete {
                    id,
                    target: docs[r].visible_id(s.pos % len).unwrap(),
                }
            };
            docs[r].apply(op.clone()).unwrap();
            seen[r].push(op.clone());
            all.push(op);
        }
        all
    }

    /// A deterministic shuffle with duplicates and no external randomness.
    fn scramble(ops: &[Op], seed: u64) -> Vec<Op> {
        let mut s = seed | 1;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let mut v: Vec<Op> = ops.to_vec();
        for op in ops {
            if next() % 3 == 0 {
                v.push(op.clone());
            }
        }
        for i in (1..v.len()).rev() {
            let j = usize::try_from(next() % (u64::try_from(i).unwrap() + 1)).unwrap();
            v.swap(i, j);
        }
        v
    }

    proptest! {
        #[test]
        fn replicas_converge_under_reorder_and_duplication(
            steps in prop::collection::vec(step_strategy(), 1..40),
            seeds in prop::collection::vec(any::<u64>(), 4),
        ) {
            let ops = run_script(&steps);
            let max = ops.iter().map(Op::id).max().unwrap();
            let expected = oracle(&ops, max, true);
            for seed in seeds {
                let mut d = Doc::new();
                for op in scramble(&ops, seed) {
                    d.apply(op).unwrap();
                }
                prop_assert_eq!(d.pending(), 0);
                prop_assert_eq!(d.text(), expected.clone());
            }
        }

        #[test]
        fn compaction_preserves_text_and_retained_points(
            steps in prop::collection::vec(step_strategy(), 1..40),
            picks in prop::collection::vec(any::<prop::sample::Index>(), 0..4),
            seed in any::<u64>(),
        ) {
            let ops = run_script(&steps);
            let max = ops.iter().map(Op::id).max().unwrap();
            let retained: Vec<Hlc> = picks.iter().map(|p| ops[p.index(ops.len())].id()).collect();
            let mut d = Doc::new();
            for op in scramble(&ops, seed) {
                d.apply(op).unwrap();
            }
            let before_text = d.text();
            let before_tombs = d.tombstones();
            let c = d.compact(max, &retained);
            prop_assert_eq!(d.text(), before_text);
            prop_assert_eq!(c.removed, before_tombs - d.tombstones());
            for p in &retained {
                prop_assert_eq!(d.text_at(*p), oracle(&ops, *p, true));
            }
            if retained.is_empty() {
                prop_assert_eq!(d.tombstones(), 0);
            }
            // Editing continues identically on a compacted and an uncompacted replica.
            let mut whole = Doc::new();
            for op in &ops {
                whole.apply(op.clone()).unwrap();
            }
            let tail_id = h(max.wall_ms + 1, 7);
            let after = d.visible_id(0);
            let tail = Op::Insert { id: tail_id, after, text: "Z".into() };
            d.apply(tail.clone()).unwrap();
            whole.apply(tail).unwrap();
            prop_assert_eq!(d.text(), whole.text());
        }

        #[test]
        fn decoder_never_panics_and_accepts_only_canonical(bytes in prop::collection::vec(any::<u8>(), 0..1200)) {
            if let Ok(op) = Op::decode(&bytes) {
                prop_assert_eq!(op.encode(), bytes);
            }
        }

        #[test]
        fn decoder_survives_mutated_valid_ops(
            steps in prop::collection::vec(step_strategy(), 1..10),
            at in any::<prop::sample::Index>(),
            val in any::<u8>(),
        ) {
            for op in run_script(&steps) {
                let mut b = op.encode();
                prop_assert_eq!(Op::decode(&b).unwrap(), op);
                let i = at.index(b.len());
                b[i] = val;
                if let Ok(back) = Op::decode(&b) {
                    prop_assert_eq!(back.encode(), b);
                }
            }
        }
    }

    // ----- measurements for ADR 0010 ----------------------------------------------------

    /// Prints the Phase 3 numbers. Run with `--nocapture`; only invariants are asserted.
    #[test]
    fn measure_ten_thousand_characters() {
        let mut s = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let mut doc = Doc::new();
        let mut ops: Vec<Op> = vec![];
        let mut clock = 0u64;
        // Random editing: grow to 10 000 visible characters with about 40% extra deletes.
        while doc.len() < 10_000 {
            clock += 1;
            let id = h(clock, 1);
            let len = doc.len();
            let op = if len > 0 && next() % 10 < 4 {
                let at = usize::try_from(next()).unwrap() % len;
                Op::Delete {
                    id,
                    target: doc.visible_id(at).unwrap(),
                }
            } else {
                let at = if len == 0 {
                    0
                } else {
                    usize::try_from(next()).unwrap() % (len + 1)
                };
                let after = if at == 0 {
                    None
                } else {
                    doc.visible_id(at - 1)
                };
                Op::Insert {
                    id,
                    after,
                    text: char::from(b'a' + u8::try_from(next() % 26).unwrap()).to_string(),
                }
            };
            doc.apply(op.clone()).unwrap();
            ops.push(op);
        }
        let n_ops = ops.len();
        use lattice_seams::Clock;
        let clock_ms = lattice_seams::SystemClock;
        let start = clock_ms.now_ms();
        let mut replay = Doc::new();
        for op in &ops {
            replay.apply(op.clone()).unwrap();
        }
        let secs = (clock_ms.now_ms() - start).max(1) as f64 / 1000.0;
        assert_eq!(replay.text(), doc.text());
        let stored = replay.stored();
        let tombs = replay.tombstones();
        let marks = [
            ops[n_ops / 4].id(),
            ops[n_ops / 2].id(),
            ops[3 * n_ops / 4].id(),
        ];
        let mut with_points = replay.clone();
        let kept = with_points.compact(h(clock, 1), &marks);
        let mut without = replay.clone();
        let dropped = without.compact(h(clock, 1), &[]);
        assert_eq!(with_points.text(), doc.text());
        assert_eq!(without.tombstones(), 0);
        println!(
            "text-crdt: ops={n_ops} visible={} stored={stored} tombstones={tombs}",
            replay.len()
        );
        println!(
            "text-crdt: elem_bytes={} heap_bytes_per_stored_elem={:.1} applied_ids={}",
            Doc::ELEM_BYTES,
            replay.element_heap_bytes() as f64 / stored as f64,
            stored
        );
        println!(
            "text-crdt: replay {:.0} ops/s ({:.3}s, debug build unless --release)",
            n_ops as f64 / secs,
            secs
        );
        println!(
            "text-crdt: compaction without points removed={} left={}; with 3 points removed={} left={}",
            dropped.removed, dropped.tombstones_left, kept.removed, kept.tombstones_left
        );
    }
}
