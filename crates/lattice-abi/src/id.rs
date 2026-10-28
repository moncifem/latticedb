//! Phantom-typed 128-bit ULID-style identifiers.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A 128-bit identifier tagged with the kind of thing it names, so ids of different kinds cannot
/// be mixed up. The layout is a 48-bit millisecond timestamp followed by 80 random bits, which
/// sorts roughly by creation time. Text form is 26 Crockford base32 characters.
pub struct Id<T> {
    raw: u128,
    _kind: PhantomData<fn() -> T>,
}

/// Why parsing an id failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdParseError {
    /// The text is not exactly 26 characters.
    #[error("an id is 26 characters, got {0}")]
    Length(usize),
    /// A character outside the Crockford alphabet.
    #[error("invalid character {0:?}")]
    Character(char),
    /// The first character encodes more than 128 bits.
    #[error("id value exceeds 128 bits")]
    Overflow,
}

impl<T> Id<T> {
    /// Wraps a raw value.
    pub const fn from_raw(raw: u128) -> Self {
        Self {
            raw,
            _kind: PhantomData,
        }
    }

    /// The raw value.
    pub const fn raw(self) -> u128 {
        self.raw
    }

    /// Builds an id from a millisecond timestamp and 80 bits of randomness (taken from `r1` and
    /// the low 16 bits of `r2`). Callers supply both from the `Clock` and `Rng` seams.
    pub fn from_parts(timestamp_ms: u64, r1: u64, r2: u64) -> Self {
        let ts = u128::from(timestamp_ms & 0xFFFF_FFFF_FFFF);
        let rand = (u128::from(r2 & 0xFFFF) << 64) | u128::from(r1);
        Self::from_raw((ts << 80) | rand)
    }

    /// Big-endian bytes.
    pub fn to_bytes(self) -> [u8; 16] {
        self.raw.to_be_bytes()
    }

    /// Reads big-endian bytes.
    pub fn from_bytes(b: [u8; 16]) -> Self {
        Self::from_raw(u128::from_be_bytes(b))
    }

    /// The timestamp component in milliseconds.
    pub fn timestamp_ms(self) -> u64 {
        u64::try_from(self.raw >> 80).unwrap_or(0)
    }

    /// Parses the 26 character text form.
    pub fn parse(text: &str) -> Result<Self, IdParseError> {
        let chars: Vec<char> = text.chars().collect();
        if chars.len() != 26 {
            return Err(IdParseError::Length(chars.len()));
        }
        let mut raw: u128 = 0;
        for (i, c) in chars.iter().enumerate() {
            let up = c.to_ascii_uppercase();
            let v = ALPHABET
                .iter()
                .position(|a| u32::from(*a) == u32::from(up))
                .ok_or(IdParseError::Character(*c))?;
            if i == 0 && v > 7 {
                return Err(IdParseError::Overflow);
            }
            raw = (raw << 5) | v as u128;
        }
        Ok(Self::from_raw(raw))
    }
}

impl<T> Clone for Id<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Id<T> {}
impl<T> PartialEq for Id<T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}
impl<T> Eq for Id<T> {}
impl<T> PartialOrd for Id<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for Id<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.raw.cmp(&other.raw)
    }
}
impl<T> Hash for Id<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

impl<T> fmt::Display for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = [b'0'; 26];
        let mut v = self.raw;
        for slot in out.iter_mut().rev() {
            *slot = ALPHABET.get((v & 31) as usize).copied().unwrap_or(b'0');
            v >>= 5;
        }
        f.write_str(&String::from_utf8_lossy(&out))
    }
}

impl<T> fmt::Debug for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    enum Thing {}

    proptest! {
        #[test]
        fn text_round_trips(raw in any::<u128>()) {
            let id = Id::<Thing>::from_raw(raw);
            let text = id.to_string();
            prop_assert_eq!(text.len(), 26);
            // The first character only has 3 usable bits, so parse accepts the values we print.
            prop_assert_eq!(Id::<Thing>::parse(&text), Ok(id));
        }

        #[test]
        fn bytes_round_trip(raw in any::<u128>()) {
            let id = Id::<Thing>::from_raw(raw);
            prop_assert_eq!(Id::<Thing>::from_bytes(id.to_bytes()), id);
        }

        #[test]
        fn later_timestamps_sort_later(a in 0u64..((1 << 47) - 1), gap in 1u64..(1 << 47), r in any::<u64>()) {
            let b = a + gap.min((1 << 47) - 1 - a);
            prop_assert!(a < b);
            prop_assert!(Id::<Thing>::from_parts(a, r, r) < Id::<Thing>::from_parts(b, 0, 0));
        }
    }

    #[test]
    fn parse_rejects_bad_input() {
        assert_eq!(Id::<Thing>::parse("short"), Err(IdParseError::Length(5)));
        assert_eq!(
            Id::<Thing>::parse("U0000000000000000000000000"),
            Err(IdParseError::Character('U'))
        );
        assert_eq!(
            Id::<Thing>::parse("80000000000000000000000000"),
            Err(IdParseError::Overflow)
        );
    }
}
