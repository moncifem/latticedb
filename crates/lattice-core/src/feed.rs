//! The change feed event model and its cursor, format version 1 (see `formats/change-feed-v1.md`).
//!
//! A committed change set becomes one [`ChangeEvent`] per entity change. Each event carries a
//! [`Cursor`], the pair `(lsn, index)`: the LSN of the log record that holds the change set and the
//! position of the change inside it. Cursor order is therefore commit order, because the log assigns
//! LSNs in commit order and never reuses one, and the index order is the payload order.
//!
//! This module is pure types and codecs. It reads no log and starts no stream; delivery, resume and
//! scoping are later work.

use crate::changeset::{ChangeSet, ChangeSetId, TurnRef};
use lattice_abi::Hlc;

/// Largest entity key an event may carry, in bytes.
pub const MAX_KEY_LEN: usize = 4096;
/// Largest value an upsert may carry, in bytes.
pub const MAX_VALUE_LEN: usize = 16 * 1024 * 1024;

const VERSION: u8 = 1;

/// Why a feed value could not be built or encoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FeedError {
    /// LSNs start at 1; 0 only exists as [`Cursor::ORIGIN`].
    #[error("an event cannot have lsn 0")]
    ZeroLsn,
    /// More changes in one change set than an index can number.
    #[error("a change set can hold at most 4294967295 events, got {0}")]
    TooManyChanges(usize),
    /// An index at or past the number of events in the change set.
    #[error("event index {index} is not below the change set size {set_len}")]
    IndexOutOfRange {
        /// The offending index.
        index: u32,
        /// Events in the change set.
        set_len: u32,
    },
    /// A key longer than [`MAX_KEY_LEN`].
    #[error("entity key of {0} bytes is too large")]
    KeyTooLarge(usize),
    /// A value longer than [`MAX_VALUE_LEN`].
    #[error("value of {0} bytes is too large")]
    ValueTooLarge(usize),
}

/// Why bytes are not a canonical feed value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FeedDecodeError {
    /// The bytes ended early.
    #[error("feed value truncated")]
    Truncated,
    /// A format version this code cannot read.
    #[error("unsupported change feed version {0}")]
    Version(u8),
    /// Bytes remained after a complete value.
    #[error("trailing bytes after feed value")]
    Trailing,
    /// A field holds an impossible or non-canonical value.
    #[error("invalid feed field: {0}")]
    Invalid(&'static str),
}

/// A position in the feed. Totally ordered by `(lsn, index)`; the derived order is that order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cursor {
    lsn: u64,
    index: u32,
}

impl Cursor {
    /// Encoded size in bytes.
    pub const ENCODED_LEN: usize = 12;
    /// Before every event. A subscriber with no history resumes after this.
    pub const ORIGIN: Cursor = Cursor { lsn: 0, index: 0 };

    /// A cursor at event `index` of the change set logged at `lsn`.
    pub fn new(lsn: u64, index: u32) -> Result<Self, FeedError> {
        if lsn == 0 {
            return Err(FeedError::ZeroLsn);
        }
        Ok(Self { lsn, index })
    }

    /// LSN of the log record holding the change set.
    pub fn lsn(self) -> u64 {
        self.lsn
    }

    /// Position of the change inside its change set.
    pub fn index(self) -> u32 {
        self.index
    }

    /// Encodes as `lsn u64 | index u32`, both big endian, so that comparing the bytes as unsigned
    /// strings gives the same order as comparing cursors.
    pub fn to_bytes(self) -> [u8; Self::ENCODED_LEN] {
        let mut o = [0u8; Self::ENCODED_LEN];
        o[..8].copy_from_slice(&self.lsn.to_be_bytes());
        o[8..].copy_from_slice(&self.index.to_be_bytes());
        o
    }

    /// Decodes exactly 12 bytes. The origin is the only cursor with LSN 0 and it has index 0.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FeedDecodeError> {
        let mut r = Reader { buf, pos: 0 };
        let c = r.cursor()?;
        r.finish()?;
        Ok(c)
    }
}

/// What happened to the entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeOp {
    /// The entity was removed.
    Delete,
    /// The entity now has this value (created or replaced).
    Upsert(Vec<u8>),
}

/// One entity change inside a change set, before it is given a cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityChange {
    /// Table, collection or other namespace of the entity.
    pub kind: u32,
    /// Key of the entity inside its namespace.
    pub key: Vec<u8>,
    /// What happened.
    pub op: ChangeOp,
}

/// One entity change of one committed change set, with its commit provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeEvent {
    /// Position of this event in the feed.
    pub cursor: Cursor,
    /// Number of events the change set produced, so a consumer can see where it ends.
    pub set_len: u32,
    /// The change set the event belongs to.
    pub change_set: ChangeSetId,
    /// Commit timestamp of the change set.
    pub hlc: Hlc,
    /// Who made the change.
    pub author: u128,
    /// The turn the write belonged to, if any.
    pub turn: Option<TurnRef>,
    /// The entity change.
    pub change: EntityChange,
}

/// Builds the events of one committed change set: one per entity change, indexed in the given order.
/// `lsn` is the LSN of the log record that holds `cs`. An empty change list yields no events.
pub fn events_for(
    lsn: u64,
    cs: &ChangeSet,
    changes: Vec<EntityChange>,
) -> Result<Vec<ChangeEvent>, FeedError> {
    if lsn == 0 {
        return Err(FeedError::ZeroLsn);
    }
    let set_len =
        u32::try_from(changes.len()).map_err(|_| FeedError::TooManyChanges(changes.len()))?;
    let mut out = Vec::with_capacity(changes.len());
    for (i, change) in changes.into_iter().enumerate() {
        check_change(&change)?;
        let index = u32::try_from(i).map_err(|_| FeedError::TooManyChanges(i))?;
        out.push(ChangeEvent {
            cursor: Cursor { lsn, index },
            set_len,
            change_set: cs.id,
            hlc: cs.hlc,
            author: cs.author,
            turn: cs.turn,
            change,
        });
    }
    Ok(out)
}

fn check_change(c: &EntityChange) -> Result<(), FeedError> {
    if c.key.len() > MAX_KEY_LEN {
        return Err(FeedError::KeyTooLarge(c.key.len()));
    }
    if let ChangeOp::Upsert(v) = &c.op
        && v.len() > MAX_VALUE_LEN
    {
        return Err(FeedError::ValueTooLarge(v.len()));
    }
    Ok(())
}

impl ChangeEvent {
    /// Encodes to the v1 layout.
    pub fn encode(&self) -> Result<Vec<u8>, FeedError> {
        if self.cursor.lsn == 0 {
            return Err(FeedError::ZeroLsn);
        }
        if self.cursor.index >= self.set_len {
            return Err(FeedError::IndexOutOfRange {
                index: self.cursor.index,
                set_len: self.set_len,
            });
        }
        check_change(&self.change)?;
        let key_len = u16::try_from(self.change.key.len())
            .map_err(|_| FeedError::KeyTooLarge(self.change.key.len()))?;
        let mut o = Vec::with_capacity(80 + self.change.key.len());
        o.push(VERSION);
        o.extend_from_slice(&self.cursor.to_bytes());
        o.extend_from_slice(&self.set_len.to_le_bytes());
        o.extend_from_slice(&self.change_set.to_bytes());
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
        o.extend_from_slice(&self.change.kind.to_le_bytes());
        o.extend_from_slice(&key_len.to_le_bytes());
        o.extend_from_slice(&self.change.key);
        match &self.change.op {
            ChangeOp::Delete => o.push(0),
            ChangeOp::Upsert(v) => {
                let len = u32::try_from(v.len()).map_err(|_| FeedError::ValueTooLarge(v.len()))?;
                o.push(1);
                o.extend_from_slice(&len.to_le_bytes());
                o.extend_from_slice(v);
            }
        }
        Ok(o)
    }

    /// Decodes the v1 layout. Only the bytes [`ChangeEvent::encode`] produces are accepted.
    pub fn decode(buf: &[u8]) -> Result<Self, FeedDecodeError> {
        let mut r = Reader { buf, pos: 0 };
        let v = r.u8()?;
        if v != VERSION {
            return Err(FeedDecodeError::Version(v));
        }
        let cursor = r.cursor()?;
        if cursor.lsn == 0 {
            return Err(FeedDecodeError::Invalid("event lsn"));
        }
        let set_len = r.u32()?;
        if cursor.index >= set_len {
            return Err(FeedDecodeError::Invalid("event index"));
        }
        let change_set = ChangeSetId::from_bytes(r.array16()?);
        let hlc = Hlc::from_bytes(&r.array16()?);
        let author = u128::from_be_bytes(r.array16()?);
        let turn = match r.u8()? {
            0 => None,
            1 => Some(TurnRef {
                session: r.u64()?,
                turn: r.u64()?,
            }),
            _ => return Err(FeedDecodeError::Invalid("turn flag")),
        };
        let kind = r.u32()?;
        let key_len = usize::from(r.u16()?);
        if key_len > MAX_KEY_LEN {
            return Err(FeedDecodeError::Invalid("key length"));
        }
        let key = r.take(key_len)?.to_vec();
        let op = match r.u8()? {
            0 => ChangeOp::Delete,
            1 => {
                let len = r.u32()? as usize;
                if len > MAX_VALUE_LEN {
                    return Err(FeedDecodeError::Invalid("value length"));
                }
                // Checked against the bytes that remain before anything is allocated.
                ChangeOp::Upsert(r.take(len)?.to_vec())
            }
            _ => return Err(FeedDecodeError::Invalid("operation")),
        };
        r.finish()?;
        Ok(Self {
            cursor,
            set_len,
            change_set,
            hlc,
            author,
            turn,
            change: EntityChange { kind, key, op },
        })
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], FeedDecodeError> {
        let end = self.pos.checked_add(n).ok_or(FeedDecodeError::Truncated)?;
        let s = self
            .buf
            .get(self.pos..end)
            .ok_or(FeedDecodeError::Truncated)?;
        self.pos = end;
        Ok(s)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], FeedDecodeError> {
        self.take(N)?
            .try_into()
            .map_err(|_| FeedDecodeError::Truncated)
    }
    fn u8(&mut self) -> Result<u8, FeedDecodeError> {
        Ok(u8::from_le_bytes(self.array()?))
    }
    fn u16(&mut self) -> Result<u16, FeedDecodeError> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, FeedDecodeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, FeedDecodeError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn array16(&mut self) -> Result<[u8; 16], FeedDecodeError> {
        self.array()
    }
    fn cursor(&mut self) -> Result<Cursor, FeedDecodeError> {
        let lsn = u64::from_be_bytes(self.array()?);
        let index = u32::from_be_bytes(self.array()?);
        if lsn == 0 && index != 0 {
            return Err(FeedDecodeError::Invalid("cursor"));
        }
        Ok(Cursor { lsn, index })
    }
    fn finish(&self) -> Result<(), FeedDecodeError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(FeedDecodeError::Trailing)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;
    use proptest::prelude::*;

    fn sample() -> ChangeEvent {
        ChangeEvent {
            cursor: Cursor::new(0x0102, 1).unwrap(),
            set_len: 3,
            change_set: ChangeSetId::from_bytes([0xA0; 16]),
            hlc: Hlc {
                wall_ms: 5,
                counter: 6,
                node: 7,
            },
            author: 9,
            turn: None,
            change: EntityChange {
                kind: 2,
                key: b"k".to_vec(),
                op: ChangeOp::Upsert(b"vv".to_vec()),
            },
        }
    }

    /// Byte vector written by hand from the format document, not produced by the encoder.
    fn sample_bytes() -> Vec<u8> {
        let mut v = vec![1];
        v.extend_from_slice(&[0, 0, 0, 0, 0, 0, 1, 2, 0, 0, 0, 1]); // cursor
        v.extend_from_slice(&[3, 0, 0, 0]); // set_len
        v.extend_from_slice(&[0xA0; 16]); // change set id
        v.extend_from_slice(&[5, 0, 0, 0, 0, 0, 0, 0, 6, 0, 0, 0, 7, 0, 0, 0]); // hlc
        v.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9]); // author
        v.push(0); // no turn
        v.extend_from_slice(&[2, 0, 0, 0]); // kind
        v.extend_from_slice(&[1, 0]); // key length
        v.push(b'k');
        v.push(1); // upsert
        v.extend_from_slice(&[2, 0, 0, 0]);
        v.extend_from_slice(b"vv");
        v
    }

    #[test]
    fn upsert_event_matches_hand_written_bytes() {
        assert_eq!(sample().encode().unwrap(), sample_bytes());
        assert_eq!(ChangeEvent::decode(&sample_bytes()).unwrap(), sample());
    }

    #[test]
    fn delete_event_with_turn_matches_hand_written_bytes() {
        let ev = ChangeEvent {
            cursor: Cursor::new(1, 0).unwrap(),
            set_len: 1,
            change_set: ChangeSetId::from_bytes([1; 16]),
            hlc: Hlc::default(),
            author: 0,
            turn: Some(TurnRef {
                session: 0x11,
                turn: 0x22,
            }),
            change: EntityChange {
                kind: 0,
                key: Vec::new(),
                op: ChangeOp::Delete,
            },
        };
        let mut b = vec![1];
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
        b.extend_from_slice(&[1, 0, 0, 0]);
        b.extend_from_slice(&[1; 16]);
        b.extend_from_slice(&[0; 16]);
        b.extend_from_slice(&[0; 16]);
        b.push(1);
        b.extend_from_slice(&[0x11, 0, 0, 0, 0, 0, 0, 0, 0x22, 0, 0, 0, 0, 0, 0, 0]);
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        b.push(0);
        assert_eq!(ev.encode().unwrap(), b);
        assert_eq!(ChangeEvent::decode(&b).unwrap(), ev);
    }

    #[test]
    fn cursor_bytes_are_big_endian() {
        let c = Cursor::new(0x0102_0304_0506_0708, 0x090A_0B0C).unwrap();
        assert_eq!(c.to_bytes(), [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(Cursor::from_bytes(&c.to_bytes()), Ok(c));
        assert_eq!(Cursor::ORIGIN.to_bytes(), [0; 12]);
        assert_eq!(Cursor::from_bytes(&[0; 12]), Ok(Cursor::ORIGIN));
    }

    #[test]
    fn cursor_rejects_bad_lengths_and_non_canonical_origin() {
        assert_eq!(
            Cursor::from_bytes(&[0; 11]),
            Err(FeedDecodeError::Truncated)
        );
        assert_eq!(Cursor::from_bytes(&[0; 13]), Err(FeedDecodeError::Trailing));
        let mut b = [0u8; 12];
        b[11] = 1;
        assert_eq!(
            Cursor::from_bytes(&b),
            Err(FeedDecodeError::Invalid("cursor"))
        );
        assert_eq!(Cursor::new(0, 0), Err(FeedError::ZeroLsn));
    }

    #[test]
    fn decoder_rejects_each_malformed_field() {
        let good = sample_bytes();
        let bad = |edit: &dyn Fn(&mut Vec<u8>)| {
            let mut v = good.clone();
            edit(&mut v);
            ChangeEvent::decode(&v)
        };
        assert_eq!(bad(&|v| v[0] = 2), Err(FeedDecodeError::Version(2)));
        assert_eq!(bad(&|v| v.push(0)), Err(FeedDecodeError::Trailing));
        assert_eq!(
            bad(&|v| v[1..13].fill(0)),
            Err(FeedDecodeError::Invalid("event lsn"))
        );
        assert_eq!(
            bad(&|v| v[12] = 3),
            Err(FeedDecodeError::Invalid("event index"))
        );
        assert_eq!(
            bad(&|v| v[65] = 2),
            Err(FeedDecodeError::Invalid("turn flag"))
        );
        assert_eq!(
            bad(&|v| v[73] = 2),
            Err(FeedDecodeError::Invalid("operation"))
        );
        assert_eq!(
            bad(&|v| v[74..78].copy_from_slice(&u32::MAX.to_le_bytes())),
            Err(FeedDecodeError::Invalid("value length"))
        );
        // A length within the bound that the input cannot satisfy is truncation, not an allocation.
        assert_eq!(
            bad(&|v| v[74..78].copy_from_slice(&1000u32.to_le_bytes())),
            Err(FeedDecodeError::Truncated)
        );
        assert_eq!(
            bad(&|v| v[70..72].copy_from_slice(&u16::MAX.to_le_bytes())),
            Err(FeedDecodeError::Invalid("key length"))
        );
        for n in 0..good.len() {
            assert!(ChangeEvent::decode(&good[..n]).is_err(), "prefix {n}");
        }
    }

    #[test]
    fn events_for_numbers_changes_in_order() {
        let cs = ChangeSet {
            id: ChangeSetId::from_bytes([7; 16]),
            hlc: Hlc {
                wall_ms: 1,
                counter: 2,
                node: 3,
            },
            author: 4,
            turn: Some(TurnRef {
                session: 5,
                turn: 6,
            }),
            parents: Vec::new(),
            payload: Vec::new(),
        };
        let changes = vec![
            EntityChange {
                kind: 1,
                key: b"a".to_vec(),
                op: ChangeOp::Delete,
            },
            EntityChange {
                kind: 1,
                key: b"b".to_vec(),
                op: ChangeOp::Upsert(Vec::new()),
            },
        ];
        let evs = events_for(40, &cs, changes).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].cursor, Cursor::new(40, 0).unwrap());
        assert_eq!(evs[1].cursor, Cursor::new(40, 1).unwrap());
        assert!(evs.iter().all(|e| e.set_len == 2 && e.change_set == cs.id));
        assert!(
            evs.iter()
                .all(|e| e.hlc == cs.hlc && e.author == 4 && e.turn == cs.turn)
        );
        assert_eq!(events_for(0, &cs, Vec::new()), Err(FeedError::ZeroLsn));
        assert_eq!(events_for(1, &cs, Vec::new()), Ok(Vec::new()));
    }

    #[test]
    fn the_value_limit_is_16_mib() {
        assert_eq!(MAX_VALUE_LEN, 16_777_216);
    }

    #[test]
    fn events_for_takes_the_largest_key_and_value_and_refuses_one_more() {
        let cs = ChangeSet::new(ChangeSetId::from_bytes([1; 16]), Vec::new());
        let upsert = |key: usize, value: usize| {
            vec![EntityChange {
                kind: 0,
                key: vec![1; key],
                op: ChangeOp::Upsert(vec![2; value]),
            }]
        };
        assert!(events_for(1, &cs, upsert(MAX_KEY_LEN, MAX_VALUE_LEN)).is_ok());
        assert_eq!(
            events_for(1, &cs, upsert(MAX_KEY_LEN + 1, 0)),
            Err(FeedError::KeyTooLarge(MAX_KEY_LEN + 1))
        );
        assert_eq!(
            events_for(1, &cs, upsert(1, MAX_VALUE_LEN + 1)),
            Err(FeedError::ValueTooLarge(MAX_VALUE_LEN + 1))
        );
    }

    #[test]
    fn decode_bounds_key_and_value_lengths_exactly() {
        let base = sample_bytes();
        // The key length field starts after the 70 bytes of fixed header; the value length after
        // the one key byte and the operation byte.
        let with_key_len = |n: usize| {
            let mut b = base[..70].to_vec();
            b.extend_from_slice(&u16::try_from(n).unwrap().to_le_bytes());
            b
        };
        assert_eq!(
            ChangeEvent::decode(&with_key_len(MAX_KEY_LEN + 1)),
            Err(FeedDecodeError::Invalid("key length"))
        );
        assert_eq!(
            ChangeEvent::decode(&with_key_len(MAX_KEY_LEN)),
            Err(FeedDecodeError::Truncated)
        );
        let with_value_len = |n: usize| {
            let mut b = base[..74].to_vec();
            b.extend_from_slice(&u32::try_from(n).unwrap().to_le_bytes());
            b
        };
        assert_eq!(
            ChangeEvent::decode(&with_value_len(MAX_VALUE_LEN + 1)),
            Err(FeedDecodeError::Invalid("value length"))
        );
        assert_eq!(
            ChangeEvent::decode(&with_value_len(MAX_VALUE_LEN)),
            Err(FeedDecodeError::Truncated)
        );
    }

    #[test]
    fn encode_refuses_what_the_format_cannot_hold() {
        let mut e = sample();
        e.change.key = vec![0; MAX_KEY_LEN + 1];
        assert_eq!(e.encode(), Err(FeedError::KeyTooLarge(MAX_KEY_LEN + 1)));
        let mut e = sample();
        e.change.op = ChangeOp::Upsert(vec![0; MAX_VALUE_LEN + 1]);
        assert_eq!(e.encode(), Err(FeedError::ValueTooLarge(MAX_VALUE_LEN + 1)));
        let mut e = sample();
        e.cursor = Cursor::new(1, 3).unwrap();
        assert_eq!(
            e.encode(),
            Err(FeedError::IndexOutOfRange {
                index: 3,
                set_len: 3
            })
        );
        let mut e = sample();
        e.cursor = Cursor::ORIGIN;
        assert_eq!(e.encode(), Err(FeedError::ZeroLsn));
    }

    fn arb_event() -> impl Strategy<Value = ChangeEvent> {
        (
            1u64..,
            1u32..,
            any::<u128>(),
            any::<(u64, u32, u32)>(),
            any::<u128>(),
            proptest::option::of(any::<(u64, u64)>()),
            any::<u32>(),
            proptest::collection::vec(any::<u8>(), 0..64),
            proptest::option::of(proptest::collection::vec(any::<u8>(), 0..64)),
            any::<u32>(),
        )
            .prop_map(
                |(lsn, set_len, id, (w, c, n), author, turn, kind, key, val, idx)| ChangeEvent {
                    cursor: Cursor {
                        lsn,
                        index: idx % set_len,
                    },
                    set_len,
                    change_set: ChangeSetId::from_bytes(id.to_be_bytes()),
                    hlc: Hlc {
                        wall_ms: w,
                        counter: c,
                        node: n,
                    },
                    author,
                    turn: turn.map(|(session, turn)| TurnRef { session, turn }),
                    change: EntityChange {
                        kind,
                        key,
                        op: val.map_or(ChangeOp::Delete, ChangeOp::Upsert),
                    },
                },
            )
    }

    fn arb_cursor() -> impl Strategy<Value = Cursor> {
        (1u64.., any::<u32>()).prop_map(|(lsn, index)| Cursor { lsn, index })
    }

    proptest! {
        #[test]
        fn event_round_trips(ev in arb_event()) {
            let bytes = ev.encode().unwrap();
            prop_assert_eq!(ChangeEvent::decode(&bytes).unwrap(), ev);
        }

        #[test]
        fn cursor_round_trips(c in arb_cursor()) {
            prop_assert_eq!(Cursor::from_bytes(&c.to_bytes()), Ok(c));
        }

        #[test]
        fn cursor_order_is_lsn_then_index(
            a in (0u64..6, 0u32..6),
            b in (0u64..6, 0u32..6),
        ) {
            // Small ranges so equal lsn values are common and the index tiebreak is exercised.
            let mk = |(lsn, index): (u64, u32)| {
                if lsn == 0 { Cursor::ORIGIN } else { Cursor { lsn, index } }
            };
            let (ca, cb) = (mk(a), mk(b));
            let key = |c: Cursor| (c.lsn(), c.index());
            prop_assert_eq!(ca.cmp(&cb), key(ca).cmp(&key(cb)));
            // The byte form sorts the same way, so a store can range-scan on it.
            prop_assert_eq!(ca.to_bytes().cmp(&cb.to_bytes()), ca.cmp(&cb));
        }

        #[test]
        fn events_of_successive_commits_are_strictly_increasing(
            sizes in proptest::collection::vec(0usize..5, 1..8),
            gaps in proptest::collection::vec(1u64..4, 8),
        ) {
            let mut lsn = 0u64;
            let mut all = Vec::new();
            for (n, gap) in sizes.iter().zip(&gaps) {
                // Checkpoint and turn records take LSNs that produce no events.
                lsn += gap;
                let cs = ChangeSet::new(ChangeSetId::from_bytes([1; 16]), Vec::new());
                let changes = (0..*n)
                    .map(|i| EntityChange { kind: 0, key: vec![i as u8], op: ChangeOp::Delete })
                    .collect();
                all.extend(events_for(lsn, &cs, changes).unwrap());
            }
            for w in all.windows(2) {
                prop_assert!(w[0].cursor < w[1].cursor);
            }
        }

        #[test]
        fn decoder_never_panics_and_accepts_only_canonical(
            bytes in proptest::collection::vec(any::<u8>(), 0..200),
        ) {
            if let Ok(ev) = ChangeEvent::decode(&bytes) {
                prop_assert_eq!(ev.encode().unwrap(), bytes.clone());
            }
            if let Ok(c) = Cursor::from_bytes(&bytes) {
                prop_assert_eq!(c.to_bytes().to_vec(), bytes);
            }
        }

        #[test]
        fn mutated_valid_events_decode_only_if_canonical(
            ev in arb_event(),
            pos in any::<usize>(),
            delta in 1u8..,
        ) {
            let mut bytes = ev.encode().unwrap();
            let at = pos % bytes.len();
            bytes[at] = bytes[at].wrapping_add(delta);
            if let Ok(back) = ChangeEvent::decode(&bytes) {
                prop_assert_eq!(back.encode().unwrap(), bytes);
                prop_assert_ne!(back, ev);
            }
        }
    }
}

#[cfg(kani)]
mod proofs;
