//! The change set envelope, format version 1 (see `formats/changeset-v1.md`).

use crate::record::{u32_at, u64_at};
use lattice_abi::{Hlc, Id};
use std::collections::{BTreeMap, BTreeSet};

/// Marker for change set ids.
#[derive(Debug, Clone, Copy)]
pub enum ChangeSetTag {}

/// Identifier of a change set. Clients assign it, which is what makes commits idempotent.
pub type ChangeSetId = Id<ChangeSetTag>;

/// The turn a write belongs to. A newer turn of the same session fences older ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TurnRef {
    /// Session (for example one agent conversation).
    pub session: u64,
    /// Turn number within the session, increasing.
    pub turn: u64,
}

/// An atomic group of changes with its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSet {
    /// Client-assigned id.
    pub id: ChangeSetId,
    /// Assigned by the kernel when the change set is prepared; clients may leave it default.
    pub hlc: Hlc,
    /// Who made the change.
    pub author: u128,
    /// The turn this write belongs to, if any.
    pub turn: Option<TurnRef>,
    /// Change sets this one causally follows.
    pub parents: Vec<ChangeSetId>,
    /// Opaque deltas (interpreted by engines in later phases).
    pub payload: Vec<u8>,
}

/// Why decoding failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The bytes ended early.
    #[error("change set truncated")]
    Truncated,
    /// A format version this code cannot read.
    #[error("unsupported change set version {0}")]
    Version(u8),
    /// Bytes remained after a complete change set.
    #[error("trailing bytes after change set")]
    Trailing,
    /// A field holds an impossible value.
    #[error("invalid change set field: {0}")]
    Invalid(&'static str),
}

/// Why a change set cannot be encoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// More parents than the format can represent (65,535).
    #[error("a change set can name at most 65535 parents, got {0}")]
    TooManyParents(usize),
    /// A payload longer than the format can represent (4 GiB).
    #[error("payload of {0} bytes is too large")]
    PayloadTooLarge(usize),
}

const VERSION: u8 = 1;

impl ChangeSet {
    /// A change set with default provenance, for tests and simple callers.
    pub fn new(id: ChangeSetId, payload: Vec<u8>) -> Self {
        Self {
            id,
            hlc: Hlc::default(),
            author: 0,
            turn: None,
            parents: Vec::new(),
            payload,
        }
    }

    /// Encodes to the v1 layout. Structural lengths are never truncated: a change set the format
    /// cannot represent is an error, not a silently different change set.
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let parents = u16::try_from(self.parents.len())
            .map_err(|_| EncodeError::TooManyParents(self.parents.len()))?;
        let len = u32::try_from(self.payload.len())
            .map_err(|_| EncodeError::PayloadTooLarge(self.payload.len()))?;
        let mut o = Vec::with_capacity(64 + self.payload.len());
        o.push(VERSION);
        o.extend_from_slice(&self.id.to_bytes());
        o.extend_from_slice(&self.hlc.to_bytes());
        o.extend_from_slice(&self.author.to_be_bytes());
        match self.turn {
            Some(t) => {
                o.push(1);
                o.extend_from_slice(&t.session.to_le_bytes());
                o.extend_from_slice(&t.turn.to_le_bytes());
            }
            None => o.push(0),
        }
        o.extend_from_slice(&parents.to_le_bytes());
        for p in &self.parents {
            o.extend_from_slice(&p.to_bytes());
        }
        o.extend_from_slice(&len.to_le_bytes());
        o.extend_from_slice(&self.payload);
        Ok(o)
    }

    /// Decodes the v1 layout, rejecting trailing bytes and unknown versions.
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader { buf, pos: 0 };
        let v = r.u8()?;
        if v != VERSION {
            return Err(DecodeError::Version(v));
        }
        let id = ChangeSetId::from_bytes(r.array16()?);
        let hlc = Hlc::from_bytes(&r.array16()?);
        let author = u128::from_be_bytes(r.array16()?);
        let turn = match r.u8()? {
            0 => None,
            1 => Some(TurnRef {
                session: r.u64()?,
                turn: r.u64()?,
            }),
            _ => return Err(DecodeError::Invalid("turn flag")),
        };
        let n = usize::from(u16::from_le_bytes(
            r.take(2)?.try_into().map_err(|_| DecodeError::Truncated)?,
        ));
        let mut parents = Vec::with_capacity(n);
        for _ in 0..n {
            parents.push(ChangeSetId::from_bytes(r.array16()?));
        }
        let len = u32_at(r.buf, r.pos).ok_or(DecodeError::Truncated)? as usize;
        r.pos += 4;
        let payload = r.take(len)?.to_vec();
        if r.pos != buf.len() {
            return Err(DecodeError::Trailing);
        }
        Ok(Self {
            id,
            hlc,
            author,
            turn,
            parents,
            payload,
        })
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], DecodeError> {
        let end = self.pos.checked_add(n).ok_or(DecodeError::Truncated)?;
        let s = self.buf.get(self.pos..end).ok_or(DecodeError::Truncated)?;
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?.first().copied().unwrap_or(0))
    }
    fn u64(&mut self) -> Result<u64, DecodeError> {
        let v = u64_at(self.buf, self.pos).ok_or(DecodeError::Truncated)?;
        self.pos += 8;
        Ok(v)
    }
    fn array16(&mut self) -> Result<[u8; 16], DecodeError> {
        self.take(16)?
            .try_into()
            .map_err(|_| DecodeError::Truncated)
    }
}

/// Payload of a `TurnBegin` record: session and turn, little endian.
pub fn encode_turn_begin(t: TurnRef) -> Vec<u8> {
    let mut o = Vec::with_capacity(16);
    o.extend_from_slice(&t.session.to_le_bytes());
    o.extend_from_slice(&t.turn.to_le_bytes());
    o
}

/// Decodes a `TurnBegin` payload.
pub fn decode_turn_begin(buf: &[u8]) -> Result<TurnRef, DecodeError> {
    if buf.len() != 16 {
        return Err(DecodeError::Invalid("turn begin length"));
    }
    Ok(TurnRef {
        session: u64_at(buf, 0).ok_or(DecodeError::Truncated)?,
        turn: u64_at(buf, 8).ok_or(DecodeError::Truncated)?,
    })
}

/// Payload of a `Checkpoint` record: what the kernel state was just before it.
///
/// Recovery recomputes the same summary from the records before the checkpoint and refuses the
/// log when they differ, which catches a log that lost or reordered records without breaking the
/// chain of checksums. Recovery still reads the whole log; skipping the prefix needs a state
/// snapshot, which a later phase adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {
    /// LSN of the last record the summary covers.
    pub through_lsn: u64,
    /// Change sets committed up to there.
    pub change_sets: u64,
    /// CRC32C over the sorted committed ids and the per-session turns.
    pub state_crc: u32,
}

/// CRC32C of the committed ids (sorted) followed by the turn table (sorted by session).
pub fn state_crc(ids: &BTreeSet<ChangeSetId>, turns: &BTreeMap<u64, u64>) -> u32 {
    let mut bytes = Vec::with_capacity(ids.len() * 16 + turns.len() * 16);
    for id in ids {
        bytes.extend_from_slice(&id.to_bytes());
    }
    for (session, turn) in turns {
        bytes.extend_from_slice(&session.to_le_bytes());
        bytes.extend_from_slice(&turn.to_le_bytes());
    }
    crate::crc::crc32c(&bytes)
}

/// Encodes a checkpoint payload (20 bytes, little endian).
pub fn encode_checkpoint(c: Checkpoint) -> Vec<u8> {
    let mut o = Vec::with_capacity(20);
    o.extend_from_slice(&c.through_lsn.to_le_bytes());
    o.extend_from_slice(&c.change_sets.to_le_bytes());
    o.extend_from_slice(&c.state_crc.to_le_bytes());
    o
}

/// Decodes a checkpoint payload.
pub fn decode_checkpoint(buf: &[u8]) -> Result<Checkpoint, DecodeError> {
    if buf.len() != 20 {
        return Err(DecodeError::Invalid("checkpoint length"));
    }
    Ok(Checkpoint {
        through_lsn: u64_at(buf, 0).ok_or(DecodeError::Truncated)?,
        change_sets: u64_at(buf, 8).ok_or(DecodeError::Truncated)?,
        state_crc: u32_at(buf, 16).ok_or(DecodeError::Truncated)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn arb_changeset() -> impl Strategy<Value = ChangeSet> {
        (
            any::<u128>(),
            any::<(u64, u32, u32)>(),
            any::<u128>(),
            proptest::option::of(any::<(u64, u64)>()),
            proptest::collection::vec(any::<u128>(), 0..4),
            proptest::collection::vec(any::<u8>(), 0..100),
        )
            .prop_map(|(id, h, author, turn, parents, payload)| ChangeSet {
                id: ChangeSetId::from_raw(id),
                hlc: Hlc {
                    wall_ms: h.0,
                    counter: h.1,
                    node: h.2,
                },
                author,
                turn: turn.map(|(session, turn)| TurnRef { session, turn }),
                parents: parents.into_iter().map(ChangeSetId::from_raw).collect(),
                payload,
            })
    }

    proptest! {
        #[test]
        fn round_trips(cs in arb_changeset()) {
            prop_assert_eq!(ChangeSet::decode(&cs.encode().unwrap()), Ok(cs));
        }

        #[test]
        fn every_strict_prefix_fails_to_decode(cs in arb_changeset(), cut in any::<usize>()) {
            let bytes = cs.encode().unwrap();
            let cut = cut % bytes.len();
            prop_assert!(ChangeSet::decode(&bytes[..cut]).is_err());
        }

        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
            let _ = ChangeSet::decode(&bytes);
        }
    }

    #[test]
    fn trailing_bytes_and_versions_are_rejected() {
        let cs = ChangeSet::new(ChangeSetId::from_raw(1), vec![1, 2, 3]);
        let mut b = cs.encode().unwrap();
        b.push(0);
        assert_eq!(ChangeSet::decode(&b), Err(DecodeError::Trailing));
        let mut v = cs.encode().unwrap();
        v[0] = 2;
        assert_eq!(ChangeSet::decode(&v), Err(DecodeError::Version(2)));
    }

    #[test]
    fn unrepresentable_lengths_are_errors_not_silent_truncation() {
        let mut cs = ChangeSet::new(ChangeSetId::from_raw(1), vec![]);
        cs.parents = vec![ChangeSetId::from_raw(2); 65_536];
        assert_eq!(cs.encode(), Err(EncodeError::TooManyParents(65_536)));
        cs.parents.truncate(65_535);
        assert!(cs.encode().is_ok());
    }

    #[test]
    fn checkpoints_round_trip_and_reject_other_lengths() {
        let c = Checkpoint {
            through_lsn: 7,
            change_sets: 5,
            state_crc: 0xDEAD_BEEF,
        };
        let bytes = encode_checkpoint(c);
        assert_eq!(bytes.len(), 20);
        assert_eq!(decode_checkpoint(&bytes), Ok(c));
        assert!(decode_checkpoint(&bytes[..19]).is_err());
        let mut longer = bytes;
        longer.push(0);
        assert!(decode_checkpoint(&longer).is_err());
    }

    #[test]
    fn the_state_summary_depends_on_ids_and_turns() {
        let id = |n: u128| ChangeSetId::from_raw(n);
        let ids: BTreeSet<ChangeSetId> = [id(1), id(2)].into();
        let turns: BTreeMap<u64, u64> = [(1, 3)].into();
        let base = state_crc(&ids, &turns);
        assert_eq!(base, state_crc(&ids, &turns));
        assert_ne!(base, state_crc(&[id(1)].into(), &turns));
        assert_ne!(base, state_crc(&ids, &[(1, 4)].into()));
        assert_ne!(base, state_crc(&ids, &BTreeMap::new()));
        assert!(![0, 1].contains(&base));
    }

    #[test]
    fn turn_begin_round_trips() {
        let t = TurnRef {
            session: 3,
            turn: 9,
        };
        assert_eq!(decode_turn_begin(&encode_turn_begin(t)), Ok(t));
        assert!(decode_turn_begin(&[0; 3]).is_err());
    }
}
