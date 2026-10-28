//! WAL record and segment format, version 1 (see `formats/wal-v1.md`).

use crate::bugs::{self, Bug};
use crate::crc::Crc32c;

/// First eight bytes of every segment file.
pub const SEGMENT_MAGIC: &[u8; 8] = b"LATLOG01";
/// Format version stored in the segment header.
pub const FORMAT_VERSION: u32 = 1;
/// Segment header: magic (8), version (4, LE), reserved (4).
pub const SEGMENT_HEADER_LEN: usize = 16;
/// Record header: payload length (4), crc (4), lsn (8), type (1), all little endian.
pub const RECORD_HEADER_LEN: usize = 17;
/// Largest payload a record may carry.
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

/// Record types. Explicit values are part of the on-disk format and must never change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
#[repr(u8)]
pub enum RecordType {
    /// A committed change set.
    ChangeSet = 1,
    /// A new turn begins for a session, fencing earlier turns.
    TurnBegin = 2,
    /// A summary of the kernel state at this point in the log.
    Checkpoint = 3,
}

impl RecordType {
    /// Parses the on-disk value.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::ChangeSet),
            2 => Some(Self::TurnBegin),
            3 => Some(Self::Checkpoint),
            _ => None,
        }
    }
}

/// A decoded record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Log sequence number, contiguous from 1.
    pub lsn: u64,
    /// Raw type byte (unknown types are preserved so newer data is not silently dropped).
    pub rtype: u8,
    /// Record body.
    pub payload: Vec<u8>,
}

/// The segment header bytes.
pub fn segment_header() -> Vec<u8> {
    let mut h = Vec::with_capacity(SEGMENT_HEADER_LEN);
    h.extend_from_slice(SEGMENT_MAGIC);
    h.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    h.extend_from_slice(&0u32.to_le_bytes());
    h
}

/// Why a segment header was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderCheck {
    /// Valid header of the supported version.
    Ok,
    /// Fewer bytes than a header (a torn creation).
    TooShort,
    /// The magic does not match.
    BadMagic,
    /// A version we do not know how to read.
    UnsupportedVersion(u32),
}

/// Validates the start of a segment file.
pub fn check_header(data: &[u8]) -> HeaderCheck {
    if data.len() < SEGMENT_HEADER_LEN {
        return HeaderCheck::TooShort;
    }
    if data.get(..8) != Some(SEGMENT_MAGIC.as_slice()) {
        return HeaderCheck::BadMagic;
    }
    match u32_at(data, 8) {
        Some(FORMAT_VERSION) => HeaderCheck::Ok,
        Some(v) => HeaderCheck::UnsupportedVersion(v),
        None => HeaderCheck::TooShort,
    }
}

fn checksum(len: u32, lsn: u64, rtype: u8, payload: &[u8]) -> u32 {
    let mut c = Crc32c::default();
    c.update(&len.to_le_bytes());
    c.update(&lsn.to_le_bytes());
    c.update(&[rtype]);
    if !bugs::active(Bug::CrcSkipsPayload) {
        c.update(payload);
    }
    c.finish()
}

/// Encodes one record.
pub fn encode_record(lsn: u64, rtype: u8, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(RECORD_HEADER_LEN + payload.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&checksum(len, lsn, rtype, payload).to_le_bytes());
    out.extend_from_slice(&lsn.to_le_bytes());
    out.push(rtype);
    out.extend_from_slice(payload);
    out
}

/// Result of decoding at an offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    /// A valid record and the offset just after it.
    Record(Record, usize),
    /// Not enough bytes for a whole record (a torn tail).
    Incomplete,
    /// The bytes are not a valid record (bad length or checksum).
    Corrupt,
}

pub(crate) fn u32_at(buf: &[u8], off: usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    Some(u32::from_le_bytes(buf.get(off..end)?.try_into().ok()?))
}

pub(crate) fn u64_at(buf: &[u8], off: usize) -> Option<u64> {
    let end = off.checked_add(8)?;
    Some(u64::from_le_bytes(buf.get(off..end)?.try_into().ok()?))
}

/// Decodes the record starting at `off`.
pub fn decode_record(buf: &[u8], off: usize) -> Decoded {
    let header_end = match off.checked_add(RECORD_HEADER_LEN) {
        Some(e) if e <= buf.len() => e,
        _ => return Decoded::Incomplete,
    };
    let (Some(len), Some(crc), Some(lsn), Some(rtype)) = (
        u32_at(buf, off),
        u32_at(buf, off + 4),
        u64_at(buf, off + 8),
        buf.get(off + 16).copied(),
    ) else {
        return Decoded::Incomplete;
    };
    if len as usize > MAX_PAYLOAD {
        return Decoded::Corrupt;
    }
    let Some(end) = header_end.checked_add(len as usize) else {
        return Decoded::Corrupt;
    };
    if end > buf.len() {
        return Decoded::Incomplete;
    }
    let payload = buf.get(header_end..end).unwrap_or(&[]);
    if checksum(len, lsn, rtype, payload) != crc {
        return Decoded::Corrupt;
    }
    Decoded::Record(
        Record {
            lsn,
            rtype,
            payload: payload.to_vec(),
        },
        end,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn the_payload_limit_and_record_type_values_are_part_of_the_format() {
        assert_eq!(MAX_PAYLOAD, 16_777_216);
        for (v, t) in [
            (1, RecordType::ChangeSet),
            (2, RecordType::TurnBegin),
            (3, RecordType::Checkpoint),
        ] {
            assert_eq!(RecordType::from_u8(v), Some(t));
            assert_eq!(t as u8, v);
        }
        assert_eq!(RecordType::from_u8(0), None);
        assert_eq!(RecordType::from_u8(4), None);
    }

    #[test]
    fn a_length_at_the_limit_is_incomplete_and_one_over_is_corrupt() {
        let header = |len: u32| {
            let mut b = Vec::new();
            b.extend_from_slice(&len.to_le_bytes());
            b.extend_from_slice(&[0; 4]);
            b.extend_from_slice(&1u64.to_le_bytes());
            b.push(1);
            b
        };
        assert_eq!(
            decode_record(&header(MAX_PAYLOAD as u32), 0),
            Decoded::Incomplete
        );
        assert_eq!(
            decode_record(&header(MAX_PAYLOAD as u32 + 1), 0),
            Decoded::Corrupt
        );
    }

    proptest! {
        #[test]
        fn encode_decode_round_trips(lsn in 1u64.., rtype in any::<u8>(), payload in proptest::collection::vec(any::<u8>(), 0..200)) {
            let bytes = encode_record(lsn, rtype, &payload);
            prop_assert_eq!(decode_record(&bytes, 0), Decoded::Record(Record { lsn, rtype, payload }, bytes.len()));
        }

        #[test]
        fn every_strict_prefix_is_incomplete(lsn in 1u64.., payload in proptest::collection::vec(any::<u8>(), 0..64), cut in any::<usize>()) {
            let bytes = encode_record(lsn, 1, &payload);
            let cut = cut % bytes.len();
            prop_assert_eq!(decode_record(&bytes[..cut], 0), Decoded::Incomplete);
        }

        #[test]
        fn any_single_bit_flip_is_detected(payload in proptest::collection::vec(any::<u8>(), 0..64), pos in any::<usize>(), bit in 0u8..8) {
            let mut bytes = encode_record(7, 1, &payload);
            let i = pos % bytes.len();
            bytes[i] ^= 1 << bit;
            // A flipped length byte may make the record look incomplete rather than corrupt,
            // but it must never decode as a valid record with different contents.
            match decode_record(&bytes, 0) {
                Decoded::Record(..) => prop_assert!(false, "a flipped bit was accepted"),
                Decoded::Incomplete | Decoded::Corrupt => {}
            }
        }
    }

    #[test]
    fn header_checks() {
        assert_eq!(check_header(&segment_header()), HeaderCheck::Ok);
        assert_eq!(check_header(b"short"), HeaderCheck::TooShort);
        let mut bad = segment_header();
        bad[0] = b'X';
        assert_eq!(check_header(&bad), HeaderCheck::BadMagic);
        let mut newer = segment_header();
        newer[8] = 9;
        assert_eq!(check_header(&newer), HeaderCheck::UnsupportedVersion(9));
    }

    #[test]
    fn oversized_length_is_corrupt() {
        let mut bytes = encode_record(1, 1, b"x");
        bytes[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode_record(&bytes, 0), Decoded::Corrupt);
    }
}

#[cfg(test)]
mod golden {
    use super::*;
    use crate::changeset::{ChangeSet, ChangeSetId, TurnRef, decode_turn_begin};
    use lattice_abi::Hlc;

    /// Written by an independent Python implementation of `formats/wal-v1.md`.
    const GOLDEN: &[u8] = include_bytes!("../../../formats/corpus/wal-v1-basic.golden");

    #[test]
    fn golden_segment_decodes_byte_exactly() {
        assert_eq!(check_header(GOLDEN), HeaderCheck::Ok);
        let mut off = SEGMENT_HEADER_LEN;
        let mut recs = Vec::new();
        while off < GOLDEN.len() {
            match decode_record(GOLDEN, off) {
                Decoded::Record(r, next) => {
                    recs.push(r);
                    off = next;
                }
                other => panic!("golden file does not decode at {off}: {other:?}"),
            }
        }
        assert_eq!(
            recs.iter().map(|r| r.lsn).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let first = ChangeSet::decode(&recs[0].payload).unwrap();
        assert_eq!(first.id, ChangeSetId::from_raw(1));
        assert_eq!(
            first.hlc,
            Hlc {
                wall_ms: 1000,
                counter: 0,
                node: 1
            }
        );
        assert_eq!(first.payload, b"hello");
        assert_eq!(
            decode_turn_begin(&recs[1].payload).unwrap(),
            TurnRef {
                session: 7,
                turn: 1
            }
        );
        let third = ChangeSet::decode(&recs[2].payload).unwrap();
        assert_eq!(
            third.turn,
            Some(TurnRef {
                session: 7,
                turn: 1
            })
        );
        assert_eq!(third.parents, vec![ChangeSetId::from_raw(1)]);
        // Re-encoding with the Rust code reproduces the golden bytes exactly.
        let mut again = segment_header();
        for r in &recs {
            again.extend_from_slice(&encode_record(r.lsn, r.rtype, &r.payload));
        }
        assert_eq!(again, GOLDEN);
    }
}
