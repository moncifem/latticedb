//! Guarded segment headers and explicit engine admission (RFC 0008).
//! Legacy WAL v1 bytes and record framing are unchanged.

use crate::crc::crc32c;
use crate::record::{HeaderCheck, SEGMENT_HEADER_LEN, check_header};

/// Guarded segment magic, deliberately rejected by legacy readers.
pub const GUARDED_MAGIC: &[u8; 8] = b"LATCAP02";
/// Guarded segment header length.
pub const GUARDED_HEADER_LEN: usize = 32;
/// Required capability for managed compound catalog/index/row payloads.
pub const MANAGED_COMPOUND: u64 = 1;
/// All required capabilities understood by this format.
pub const KNOWN_CAPABILITIES: u64 = MANAGED_COMPOUND;

/// Segment provenance supplied to a recovery consumer, separate from record bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoveryContext {
    /// Engine capabilities the segment requires its consumer to understand.
    pub required_capabilities: u64,
}

/// Parsed header; callers still check their own supported engine capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentHeader {
    /// Byte offset of the first record.
    pub length: usize,
    /// Required engine capabilities.
    pub required_capabilities: u64,
}

/// A segment header is incomplete or must be refused without repair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderError {
    /// No complete header exists yet; only the newest empty creation may be repaired.
    Incomplete,
    /// Complete header bytes are invalid or unsupported.
    Invalid(String),
}

/// Encodes the guarded header. Zero or unknown requirements are invalid.
pub fn guarded_header(required: u64) -> Result<Vec<u8>, HeaderError> {
    if required == 0 || required & !KNOWN_CAPABILITIES != 0 {
        return Err(HeaderError::Invalid("invalid required capabilities".into()));
    }
    let mut bytes = Vec::with_capacity(GUARDED_HEADER_LEN);
    bytes.extend_from_slice(GUARDED_MAGIC);
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&required.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&crc32c(&bytes).to_le_bytes());
    Ok(bytes)
}

/// Parses a bounded prefix. No allocations depend on a declared length or capability count.
pub fn decode_header(bytes: &[u8]) -> Result<SegmentHeader, HeaderError> {
    // Reject a complete unsupported version before an otherwise short header is repairable.
    if bytes.get(..8) == Some(GUARDED_MAGIC.as_slice()) {
        if let Some(version) = bytes.get(8..12)
            && version != 2u32.to_le_bytes().as_slice()
        {
            return Err(HeaderError::Invalid("unsupported guarded version".into()));
        }
        if let Some(length) = bytes.get(12..14)
            && length != 32u16.to_le_bytes().as_slice()
        {
            return Err(HeaderError::Invalid("unsupported guarded length".into()));
        }
    }
    if bytes.len() < SEGMENT_HEADER_LEN {
        return Err(HeaderError::Incomplete);
    }
    if bytes.get(..8) != Some(GUARDED_MAGIC.as_slice()) {
        return match check_header(bytes) {
            HeaderCheck::Ok => Ok(SegmentHeader {
                length: SEGMENT_HEADER_LEN,
                required_capabilities: 0,
            }),
            other => Err(HeaderError::Invalid(format!("legacy header: {other:?}"))),
        };
    }
    // Unknown layouts must refuse before the torn-creation repair path can interpret them.
    if bytes.get(8..12) != Some(2u32.to_le_bytes().as_slice())
        || bytes.get(12..16) != Some([32, 0, 0, 0].as_slice())
    {
        return Err(HeaderError::Invalid(
            "guarded version, length or reserved field".into(),
        ));
    }
    if bytes.len() < GUARDED_HEADER_LEN {
        return Err(HeaderError::Incomplete);
    }
    let prefix = bytes.get(..28).ok_or(HeaderError::Incomplete)?;
    if bytes.get(24..28) != Some([0; 4].as_slice())
        || bytes.get(28..32) != Some(crc32c(prefix).to_le_bytes().as_slice())
    {
        return Err(HeaderError::Invalid(
            "guarded version, length, reserved fields or checksum".into(),
        ));
    }
    let required = bytes
        .get(16..24)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or(HeaderError::Incomplete)?;
    if required == 0 || required & !KNOWN_CAPABILITIES != 0 {
        return Err(HeaderError::Invalid(
            "unknown or empty required capabilities".into(),
        ));
    }
    Ok(SegmentHeader {
        length: GUARDED_HEADER_LEN,
        required_capabilities: required,
    })
}
