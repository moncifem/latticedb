//! The payload of a change set that writes rows: which table and key, and the new value or a delete.
//!
//! Layout (all integers little endian): the magic `LROW`, a one byte version, a `u32` count, then per
//! write `u32 table`, `u32 key length`, the key, a one byte kind (0 delete, 1 put), and for a put a
//! `u32 value length` and the value. Decoding is strict: it rejects truncation, an unknown kind and
//! trailing bytes, so a damaged payload is an error and never a different set of writes.

/// A row address: table id and primary key bytes.
pub type RowKey = (u32, Vec<u8>);

/// One write: `None` deletes the row.
pub type RowWrite = (RowKey, Option<Vec<u8>>);

/// The first bytes of every row-write payload; other engines use other magics.
pub const MAGIC: [u8; 4] = *b"LROW";
const VERSION: u8 = 1;

/// Why a payload could not be read as row writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowOpsError {
    /// The bytes end inside a field.
    Truncated,
    /// A kind byte other than 0 or 1.
    BadKind(u8),
    /// A format version this code cannot read.
    UnsupportedVersion(u8),
    /// Bytes remain after the last write.
    Trailing,
    /// A field is longer than the format can hold.
    TooLarge,
}

/// Whether `payload` is a row-write payload.
pub fn is_row_payload(payload: &[u8]) -> bool {
    payload.get(..4) == Some(MAGIC.as_slice())
}

/// Encodes writes, or fails if a count or length does not fit in the format.
pub fn encode(writes: &[RowWrite]) -> Result<Vec<u8>, RowOpsError> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.extend_from_slice(
        &u32::try_from(writes.len())
            .map_err(|_| RowOpsError::TooLarge)?
            .to_le_bytes(),
    );
    for ((table, key), value) in writes {
        out.extend_from_slice(&table.to_le_bytes());
        out.extend_from_slice(
            &u32::try_from(key.len())
                .map_err(|_| RowOpsError::TooLarge)?
                .to_le_bytes(),
        );
        out.extend_from_slice(key);
        match value {
            None => out.push(0),
            Some(v) => {
                out.push(1);
                out.extend_from_slice(
                    &u32::try_from(v.len())
                        .map_err(|_| RowOpsError::TooLarge)?
                        .to_le_bytes(),
                );
                out.extend_from_slice(v);
            }
        }
    }
    Ok(out)
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], RowOpsError> {
        let end = self.at.checked_add(n).ok_or(RowOpsError::Truncated)?;
        let slice = self.bytes.get(self.at..end).ok_or(RowOpsError::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, RowOpsError> {
        self.take(1)?.first().copied().ok_or(RowOpsError::Truncated)
    }

    fn u32(&mut self) -> Result<u32, RowOpsError> {
        let b: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| RowOpsError::Truncated)?;
        Ok(u32::from_le_bytes(b))
    }
}

/// Decodes a row-write payload.
pub fn decode(payload: &[u8]) -> Result<Vec<RowWrite>, RowOpsError> {
    let mut r = Reader {
        bytes: payload,
        at: 0,
    };
    if r.take(4)? != MAGIC.as_slice() {
        return Err(RowOpsError::Truncated);
    }
    let version = r.u8()?;
    if version != VERSION {
        return Err(RowOpsError::UnsupportedVersion(version));
    }
    let count = r.u32()?;
    let mut out = Vec::new();
    for _ in 0..count {
        let table = r.u32()?;
        let key_len = r.u32()? as usize;
        let key = r.take(key_len)?.to_vec();
        let value = match r.u8()? {
            0 => None,
            1 => {
                let len = r.u32()? as usize;
                Some(r.take(len)?.to_vec())
            }
            other => return Err(RowOpsError::BadKind(other)),
        };
        out.push(((table, key), value));
    }
    if r.at != payload.len() {
        return Err(RowOpsError::Trailing);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;
    use proptest::prelude::*;

    fn write() -> impl Strategy<Value = RowWrite> {
        (
            any::<u32>(),
            proptest::collection::vec(any::<u8>(), 0..20),
            proptest::option::of(proptest::collection::vec(any::<u8>(), 0..40)),
        )
            .prop_map(|(t, k, v)| ((t, k), v))
    }

    proptest! {
        #[test]
        fn encode_then_decode_is_identity(ws in proptest::collection::vec(write(), 0..8)) {
            prop_assert_eq!(decode(&encode(&ws).unwrap()), Ok(ws));
        }

        #[test]
        fn arbitrary_bytes_never_panic_and_a_success_re_encodes_identically(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
            if let Ok(ws) = decode(&bytes) {
                prop_assert_eq!(encode(&ws).unwrap(), bytes);
            }
        }

        #[test]
        fn every_truncation_is_an_error(ws in proptest::collection::vec(write(), 1..5)) {
            let bytes = encode(&ws).unwrap();
            for cut in 0..bytes.len() {
                prop_assert!(decode(&bytes[..cut]).is_err(), "cut at {}", cut);
            }
        }
    }

    #[test]
    fn trailing_bytes_and_unknown_kinds_are_refused() {
        let mut bytes = encode(&[((1, vec![1]), Some(vec![2]))]).unwrap();
        let ok = bytes.clone();
        bytes.push(0);
        assert_eq!(decode(&bytes), Err(RowOpsError::Trailing));
        let mut bad = ok;
        let kind_at = 4 + 1 + 4 + 4 + 4 + 1 + 1;
        bad[kind_at - 1] = 9;
        assert_eq!(decode(&bad), Err(RowOpsError::BadKind(9)));
    }

    #[test]
    fn other_payloads_are_not_row_payloads() {
        assert!(!is_row_payload(b"ledger\n{}"));
        assert!(is_row_payload(&encode(&[]).unwrap()));
    }
}
