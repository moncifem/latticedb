//! Row operations carried in a change set payload (`formats/rowops-v0.md`, ADR 0012).

/// Largest number of operations in one change set payload.
pub const MAX_OPS: usize = 65_535;
/// Longest key.
pub const MAX_KEY: usize = 4_096;
/// Longest value.
pub const MAX_VALUE: usize = 8 * 1024 * 1024;

const VERSION: u8 = 1;
const KIND_PUT: u8 = 1;
const KIND_DELETE: u8 = 2;

/// One write to one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowOp {
    /// Insert or replace a row.
    Put {
        /// Numeric table id from the catalog.
        table: u32,
        /// The row key.
        key: Vec<u8>,
        /// The row value.
        value: Vec<u8>,
    },
    /// Delete a row.
    Delete {
        /// Numeric table id from the catalog.
        table: u32,
        /// The row key.
        key: Vec<u8>,
    },
}

/// Why a payload did not encode or decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowOpsError {
    /// There are no operations.
    Empty,
    /// More operations than [`MAX_OPS`].
    TooManyOps,
    /// A key longer than [`MAX_KEY`].
    KeyTooLong,
    /// A value longer than [`MAX_VALUE`].
    ValueTooLong,
    /// The payload ended in the middle of a field.
    Truncated,
    /// Bytes remain after the last operation.
    TrailingBytes,
    /// A version this code does not know.
    UnknownVersion(u8),
    /// An operation kind this code does not know.
    UnknownKind(u8),
}

/// Encodes operations, or says why they cannot be encoded.
pub fn encode(ops: &[RowOp]) -> Result<Vec<u8>, RowOpsError> {
    if ops.is_empty() {
        return Err(RowOpsError::Empty);
    }
    if ops.len() > MAX_OPS {
        return Err(RowOpsError::TooManyOps);
    }
    let mut out = vec![VERSION];
    out.extend_from_slice(&(ops.len() as u32).to_le_bytes());
    for op in ops {
        let (kind, table, key, value) = match op {
            RowOp::Put { table, key, value } => (KIND_PUT, *table, key, Some(value)),
            RowOp::Delete { table, key } => (KIND_DELETE, *table, key, None),
        };
        if key.len() > MAX_KEY {
            return Err(RowOpsError::KeyTooLong);
        }
        out.push(kind);
        out.extend_from_slice(&table.to_le_bytes());
        out.extend_from_slice(&(key.len() as u16).to_le_bytes());
        out.extend_from_slice(key);
        if let Some(value) = value {
            if value.len() > MAX_VALUE {
                return Err(RowOpsError::ValueTooLong);
            }
            out.extend_from_slice(&(value.len() as u32).to_le_bytes());
            out.extend_from_slice(value);
        }
    }
    Ok(out)
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], RowOpsError> {
        if self.rest.len() < n {
            return Err(RowOpsError::Truncated);
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, RowOpsError> {
        self.take(1)?.first().copied().ok_or(RowOpsError::Truncated)
    }

    fn u16(&mut self) -> Result<u16, RowOpsError> {
        let b: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| RowOpsError::Truncated)?;
        Ok(u16::from_le_bytes(b))
    }

    fn u32(&mut self) -> Result<u32, RowOpsError> {
        let b: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| RowOpsError::Truncated)?;
        Ok(u32::from_le_bytes(b))
    }
}

/// Decodes a payload. Total: every input gives operations or an error, and no length is trusted
/// before it has been checked against the bytes that remain.
pub fn decode(payload: &[u8]) -> Result<Vec<RowOp>, RowOpsError> {
    let mut r = Reader { rest: payload };
    match r.u8()? {
        VERSION => {}
        v => return Err(RowOpsError::UnknownVersion(v)),
    }
    let count = r.u32()? as usize;
    if count == 0 {
        return Err(RowOpsError::Empty);
    }
    if count > MAX_OPS {
        return Err(RowOpsError::TooManyOps);
    }
    // Do not reserve from the claimed count: a short payload could claim 65,535 operations.
    let mut ops = Vec::new();
    for _ in 0..count {
        let kind = r.u8()?;
        let table = r.u32()?;
        let key_len = usize::from(r.u16()?);
        if key_len > MAX_KEY {
            return Err(RowOpsError::KeyTooLong);
        }
        let key = r.take(key_len)?.to_vec();
        match kind {
            KIND_PUT => {
                let value_len = r.u32()? as usize;
                if value_len > MAX_VALUE {
                    return Err(RowOpsError::ValueTooLong);
                }
                let value = r.take(value_len)?.to_vec();
                ops.push(RowOp::Put { table, key, value });
            }
            KIND_DELETE => ops.push(RowOp::Delete { table, key }),
            k => return Err(RowOpsError::UnknownKind(k)),
        }
    }
    if !r.rest.is_empty() {
        return Err(RowOpsError::TrailingBytes);
    }
    Ok(ops)
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

    fn op() -> impl Strategy<Value = RowOp> {
        let key = proptest::collection::vec(any::<u8>(), 0..40);
        let value = proptest::collection::vec(any::<u8>(), 0..80);
        prop_oneof![
            (any::<u32>(), key.clone(), value).prop_map(|(table, key, value)| RowOp::Put {
                table,
                key,
                value
            }),
            (any::<u32>(), key).prop_map(|(table, key)| RowOp::Delete { table, key }),
        ]
    }

    proptest! {
        #[test]
        fn what_is_encoded_decodes_to_the_same_operations(ops in proptest::collection::vec(op(), 1..12)) {
            let bytes = encode(&ops).unwrap();
            prop_assert_eq!(decode(&bytes).unwrap(), ops);
        }

        #[test]
        fn arbitrary_bytes_never_panic_and_a_success_re_encodes_identically(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
            if let Ok(ops) = decode(&bytes) {
                prop_assert_eq!(encode(&ops).unwrap(), bytes);
            }
        }

        #[test]
        fn every_proper_prefix_of_a_valid_payload_is_rejected(ops in proptest::collection::vec(op(), 1..6)) {
            let bytes = encode(&ops).unwrap();
            for cut in 0..bytes.len() {
                prop_assert!(decode(&bytes[..cut]).is_err(), "prefix of {cut} bytes decoded");
            }
        }
    }

    #[test]
    fn trailing_bytes_unknown_versions_and_kinds_and_empty_lists_are_errors() {
        let good = encode(&[RowOp::Delete {
            table: 1,
            key: vec![7],
        }])
        .unwrap();
        let mut more = good.clone();
        more.push(0);
        assert_eq!(decode(&more), Err(RowOpsError::TrailingBytes));
        let mut v = good.clone();
        v[0] = 2;
        assert_eq!(decode(&v), Err(RowOpsError::UnknownVersion(2)));
        let mut k = good.clone();
        k[5] = 9;
        assert_eq!(decode(&k), Err(RowOpsError::UnknownKind(9)));
        assert_eq!(encode(&[]), Err(RowOpsError::Empty));
        assert_eq!(decode(&[1, 0, 0, 0, 0]), Err(RowOpsError::Empty));
    }

    #[test]
    fn limits_are_enforced_on_both_sides() {
        let long_key = RowOp::Delete {
            table: 1,
            key: vec![0; MAX_KEY + 1],
        };
        assert_eq!(encode(&[long_key]), Err(RowOpsError::KeyTooLong));
        let ok_key = RowOp::Delete {
            table: 1,
            key: vec![0; MAX_KEY],
        };
        assert!(encode(&[ok_key]).is_ok());
        let big = RowOp::Put {
            table: 1,
            key: vec![],
            value: vec![0; MAX_VALUE + 1],
        };
        assert_eq!(encode(&[big]), Err(RowOpsError::ValueTooLong));
        // A short payload claiming a huge count fails on the missing bytes, not on memory.
        let mut lying = vec![1];
        lying.extend_from_slice(&(MAX_OPS as u32).to_le_bytes());
        assert_eq!(decode(&lying), Err(RowOpsError::Truncated));
        let mut too_many = vec![1];
        too_many.extend_from_slice(&((MAX_OPS as u32) + 1).to_le_bytes());
        assert_eq!(decode(&too_many), Err(RowOpsError::TooManyOps));
    }
}
