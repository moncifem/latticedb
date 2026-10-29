//! Runtime values, their text forms and their comparison.

use std::cmp::Ordering;
use std::fmt::Write as _;

use lattice_sql::plan::SqlType;

use crate::decimal::{Decimal, NumError};
use crate::error::ExecError;

/// A value of a row or an expression result.
#[derive(Clone, Debug, PartialEq)]
pub enum Datum {
    /// SQL `NULL`.
    Null,
    /// `boolean`.
    Bool(bool),
    /// `smallint`, `integer` or `bigint`; the column type bounds the range.
    Int(i64),
    /// `double precision`.
    Float(f64),
    /// `numeric`.
    Numeric(Decimal),
    /// `text` and `varchar(n)`.
    Text(String),
    /// `bytea`.
    Bytes(Vec<u8>),
    /// `uuid`.
    Uuid(u128),
    /// `date`: days since 1970-01-01.
    Date(i32),
    /// `timestamptz`: microseconds since 1970-01-01 00:00:00 UTC.
    Timestamp(i64),
    /// `jsonb`, kept as the validated text it was written as.
    Json(String),
}

impl Datum {
    /// Whether the value is `NULL`.
    pub fn is_null(&self) -> bool {
        matches!(self, Datum::Null)
    }

    /// Rough size in bytes for memory accounting.
    pub(crate) fn size(&self) -> u64 {
        let heap = match self {
            Datum::Text(s) | Datum::Json(s) => s.len(),
            Datum::Bytes(b) => b.len(),
            _ => 0,
        };
        32 + heap as u64
    }

    /// The text PostgreSQL prints for the value in the text result format.
    pub fn to_pg_text(&self) -> Option<String> {
        Some(match self {
            Datum::Null => return None,
            Datum::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
            Datum::Int(i) => i.to_string(),
            Datum::Float(f) => format_float(*f),
            Datum::Numeric(d) => d.to_string(),
            Datum::Text(s) | Datum::Json(s) => s.clone(),
            Datum::Bytes(b) => {
                let mut s = String::with_capacity(2 + b.len() * 2);
                s.push_str("\\x");
                for byte in b {
                    let _ = write!(s, "{byte:02x}");
                }
                s
            }
            Datum::Uuid(u) => format_uuid(*u),
            Datum::Date(d) => format_date(*d),
            Datum::Timestamp(t) => format_timestamp(*t),
        })
    }
}

/// `1.5e300` style, as PostgreSQL prints a double.
pub(crate) fn format_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    let a = f.abs();
    if a != 0.0 && !(1e-4..1e15).contains(&a) {
        let s = format!("{f:e}");
        if let Some((m, e)) = s.split_once('e') {
            let (sign, digits) = match e.strip_prefix('-') {
                Some(d) => ('-', d),
                None => ('+', e),
            };
            return format!("{m}e{sign}{digits:0>2}");
        }
    }
    format!("{f}")
}

pub(crate) fn format_uuid(u: u128) -> String {
    let h = format!("{u:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        h.get(0..8).unwrap_or(""),
        h.get(8..12).unwrap_or(""),
        h.get(12..16).unwrap_or(""),
        h.get(16..20).unwrap_or(""),
        h.get(20..).unwrap_or("")
    )
}

// ----- civil calendar (proleptic Gregorian) -----

/// Days since 1970-01-01 for a civil date.
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of a day count.
pub(crate) fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(y) => 29,
        _ => 28,
    }
}

const MICROS_PER_DAY: i64 = 86_400_000_000;
/// Years the subset accepts (PostgreSQL accepts more).
const YEAR_RANGE: std::ops::RangeInclusive<i64> = 1..=9999;

pub(crate) fn format_date(days: i32) -> String {
    let (y, m, d) = civil_from_days(i64::from(days));
    format!("{y:04}-{m:02}-{d:02}")
}

pub(crate) fn format_timestamp(micros: i64) -> String {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let rem = micros.rem_euclid(MICROS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s, us) = (
        rem / 3_600_000_000,
        rem / 60_000_000 % 60,
        rem / 1_000_000 % 60,
        rem % 1_000_000,
    );
    let mut out = format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}");
    if us != 0 {
        let frac = format!("{us:06}");
        out.push('.');
        out.push_str(frac.trim_end_matches('0'));
    }
    out.push_str("+00");
    out
}

fn parse_date_part(s: &str) -> Option<i64> {
    let mut it = s.split('-');
    let (y, m, d) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() {
        return None;
    }
    let num = |t: &str| -> Option<i64> {
        (!t.is_empty() && t.bytes().all(|b| b.is_ascii_digit())).then(|| t.parse().ok())?
    };
    let (y, m, d) = (num(y)?, num(m)?, num(d)?);
    if !YEAR_RANGE.contains(&y) || !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// Parses a `date` (a timestamp's date part is accepted, as in PostgreSQL).
pub(crate) fn parse_date(s: &str) -> Option<i32> {
    let t = s.trim();
    let date = t.split([' ', 'T']).next()?;
    i32::try_from(parse_date_part(date)?).ok()
}

/// Parses `YYYY-MM-DD[ T]HH:MM[:SS[.ffffff]][Z|+hh[:mm]|-hh[:mm]]`; a bare date is midnight UTC.
pub(crate) fn parse_timestamp(s: &str) -> Option<i64> {
    let t = s.trim();
    let (date, rest) = match t.find([' ', 'T']) {
        Some(at) => (t.get(..at)?, t.get(at + 1..)?.trim()),
        None => (t, ""),
    };
    let days = parse_date_part(date)?;
    let mut micros = days.checked_mul(MICROS_PER_DAY)?;
    if rest.is_empty() {
        return Some(micros);
    }
    let (clock, offset_minutes) = split_offset(rest)?;
    let mut parts = clock.split(':');
    let h: i64 = parts.next()?.parse().ok()?;
    let mi: i64 = parts.next()?.parse().ok()?;
    let (sec, frac) = match parts.next() {
        Some(sp) => match sp.split_once('.') {
            Some((a, b)) => (a.parse::<i64>().ok()?, b),
            None => (sp.parse::<i64>().ok()?, ""),
        },
        None => (0, ""),
    };
    if parts.next().is_some()
        || !(0..=24).contains(&h)
        || !(0..60).contains(&mi)
        || !(0..=60).contains(&sec)
    {
        return None;
    }
    if !frac.bytes().all(|b| b.is_ascii_digit())
        || frac.get(6..).is_some_and(|x| x.bytes().any(|b| b != b'0'))
    {
        // PostgreSQL rounds beyond microseconds; the subset refuses them.
        return None;
    }
    let mut us: i64 = 0;
    for (i, b) in frac.bytes().take(6).enumerate() {
        us += i64::from(b - b'0') * 10i64.pow(5 - u32::try_from(i).ok()?);
    }
    micros = micros.checked_add(h * 3_600_000_000 + mi * 60_000_000 + sec * 1_000_000 + us)?;
    micros = micros.checked_sub(offset_minutes.checked_mul(60_000_000)?)?;
    Some(micros)
}

fn split_offset(s: &str) -> Option<(&str, i64)> {
    if let Some(body) = s.strip_suffix(['Z', 'z']) {
        return Some((body.trim_end(), 0));
    }
    if let Some(at) = s.rfind(['+', '-']) {
        let (body, off) = s.split_at(at);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let off = off.get(1..)?;
        let (hh, mm) = match off.split_once(':') {
            Some((a, b)) => (a, b),
            None if off.len() > 2 => (off.get(..2)?, off.get(2..)?),
            None => (off, "0"),
        };
        let (hh, mm): (i64, i64) = (hh.parse().ok()?, mm.parse().ok()?);
        return Some((body.trim_end(), sign * (hh * 60 + mm)));
    }
    Some((s, 0))
}

/// Days of a date as microseconds, for comparing a date with a timestamp.
pub(crate) fn date_to_micros(d: i32) -> i64 {
    i64::from(d) * MICROS_PER_DAY
}

pub(crate) fn micros_to_date(t: i64) -> Option<i32> {
    i32::try_from(t.div_euclid(MICROS_PER_DAY)).ok()
}

pub(crate) fn parse_uuid(s: &str) -> Option<u128> {
    let t = s.trim();
    let t = t
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .unwrap_or(t);
    let hex: String = t.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let hyphens: Vec<usize> = t.match_indices('-').map(|(i, _)| i).collect();
    if !hyphens.is_empty() && hyphens != [8, 13, 18, 23] {
        return None;
    }
    u128::from_str_radix(&hex, 16).ok()
}

fn parse_bytea(s: &str) -> Option<Vec<u8>> {
    let hex = s.trim().strip_prefix("\\x")?;
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let b = hex.as_bytes();
    for pair in b.chunks(2) {
        let hi = (*pair.first()? as char).to_digit(16)?;
        let lo = (*pair.get(1)? as char).to_digit(16)?;
        out.push(u8::try_from(hi * 16 + lo).ok()?);
    }
    Some(out)
}

pub(crate) fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "t" | "true" | "y" | "yes" | "on" | "1" => Some(true),
        "f" | "false" | "n" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

// ----- jsonb validation -----

const MAX_JSON_DEPTH: usize = 64;

/// Whether `s` is one JSON value (RFC 8259), nested at most 64 deep.
pub(crate) fn json_is_valid(s: &str) -> bool {
    let b = s.as_bytes();
    let mut p = JsonScan { b, i: 0 };
    p.ws();
    p.value(0) && {
        p.ws();
        p.i == b.len()
    }
}

struct JsonScan<'a> {
    b: &'a [u8],
    i: usize,
}

impl JsonScan<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }
    fn lit(&mut self, word: &str) -> bool {
        if self.b.get(self.i..self.i + word.len()) == Some(word.as_bytes()) {
            self.i += word.len();
            true
        } else {
            false
        }
    }
    fn value(&mut self, depth: usize) -> bool {
        if depth > MAX_JSON_DEPTH {
            return false;
        }
        match self.peek() {
            Some(b'{') => {
                self.i += 1;
                self.ws();
                if self.peek() == Some(b'}') {
                    self.i += 1;
                    return true;
                }
                loop {
                    self.ws();
                    if !self.string() {
                        return false;
                    }
                    self.ws();
                    if self.peek() != Some(b':') {
                        return false;
                    }
                    self.i += 1;
                    self.ws();
                    if !self.value(depth + 1) {
                        return false;
                    }
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return true;
                        }
                        _ => return false,
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                self.ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                    return true;
                }
                loop {
                    self.ws();
                    if !self.value(depth + 1) {
                        return false;
                    }
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return true;
                        }
                        _ => return false,
                    }
                }
            }
            Some(b'"') => self.string(),
            Some(b't') => self.lit("true"),
            Some(b'f') => self.lit("false"),
            Some(b'n') => self.lit("null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => false,
        }
    }
    fn string(&mut self) -> bool {
        if self.peek() != Some(b'"') {
            return false;
        }
        self.i += 1;
        loop {
            match self.peek() {
                None => return false,
                Some(b'"') => {
                    self.i += 1;
                    return true;
                }
                Some(b'\\') => {
                    self.i += 1;
                    match self.peek() {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => self.i += 1,
                        Some(b'u') => {
                            self.i += 1;
                            for _ in 0..4 {
                                if !self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                                    return false;
                                }
                                self.i += 1;
                            }
                        }
                        _ => return false,
                    }
                }
                Some(c) if c < 0x20 => return false,
                Some(_) => self.i += 1,
            }
        }
    }
    fn digits(&mut self) -> bool {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        self.i > start
    }
    fn number(&mut self) -> bool {
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return false,
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            if !self.digits() {
                return false;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !self.digits() {
                return false;
            }
        }
        true
    }
}

// ----- conversion from text -----

fn invalid(ty: SqlType, s: &str) -> ExecError {
    ExecError::eval(
        "22P02",
        format!("invalid input syntax for type {ty}: \"{s}\""),
    )
}

/// The range of an integer type.
pub(crate) fn int_bounds(ty: SqlType) -> Option<(i64, i64)> {
    match ty {
        SqlType::SmallInt => Some((i64::from(i16::MIN), i64::from(i16::MAX))),
        SqlType::Integer => Some((i64::from(i32::MIN), i64::from(i32::MAX))),
        SqlType::BigInt => Some((i64::MIN, i64::MAX)),
        _ => None,
    }
}

/// Checks an integer against its type; `22003` when it does not fit.
pub(crate) fn check_int(v: i128, ty: SqlType) -> Result<i64, ExecError> {
    let (lo, hi) = int_bounds(ty).unwrap_or((i64::MIN, i64::MAX));
    if v < i128::from(lo) || v > i128::from(hi) {
        return Err(ExecError::eval("22003", format!("{ty} out of range")));
    }
    i64::try_from(v).map_err(|_| ExecError::eval("22003", format!("{ty} out of range")))
}

/// Reads the text form of a value of type `ty` (`22P02` when it is not one).
pub(crate) fn parse_text(s: &str, ty: SqlType) -> Result<Datum, ExecError> {
    Ok(match ty {
        SqlType::Boolean => Datum::Bool(parse_bool(s).ok_or_else(|| invalid(ty, s))?),
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt => {
            let t = s.trim();
            let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid(ty, s));
            }
            let v: i128 = t.parse().map_err(|_| {
                ExecError::eval(
                    "22003",
                    format!("value \"{s}\" is out of range for type {ty}"),
                )
            })?;
            Datum::Int(check_int(v, ty).map_err(|_| {
                ExecError::eval(
                    "22003",
                    format!("value \"{s}\" is out of range for type {ty}"),
                )
            })?)
        }
        SqlType::Numeric => Datum::Numeric(Decimal::parse(s).map_err(|e| match e {
            NumError::Syntax => invalid(ty, s),
            other => other.into(),
        })?),
        SqlType::DoublePrecision => {
            let t = s.trim();
            let f: f64 = t.parse().map_err(|_| invalid(ty, s))?;
            let named = t
                .bytes()
                .any(|b| b.is_ascii_alphabetic() && !matches!(b, b'e' | b'E'));
            if f.is_infinite() && !named {
                return Err(ExecError::eval(
                    "22003",
                    format!("\"{s}\" is out of range for type double precision"),
                ));
            }
            Datum::Float(f)
        }
        SqlType::Text | SqlType::Unknown => Datum::Text(s.to_string()),
        SqlType::Bytea => Datum::Bytes(parse_bytea(s).ok_or_else(|| invalid(ty, s))?),
        SqlType::Uuid => Datum::Uuid(parse_uuid(s).ok_or_else(|| invalid(ty, s))?),
        SqlType::Date => Datum::Date(parse_date(s).ok_or_else(|| {
            ExecError::eval(
                "22007",
                format!("invalid input syntax for type date: \"{s}\""),
            )
        })?),
        SqlType::Timestamptz => Datum::Timestamp(parse_timestamp(s).ok_or_else(|| {
            ExecError::eval(
                "22007",
                format!("invalid input syntax for type timestamp with time zone: \"{s}\""),
            )
        })?),
        SqlType::Jsonb => {
            if !json_is_valid(s) {
                return Err(ExecError::eval(
                    "22P02",
                    format!("invalid input syntax for type json: \"{s}\""),
                ));
            }
            Datum::Json(s.trim().to_string())
        }
    })
}

// ----- comparison -----

/// Compares two non-null values, promoting numerics (and `date` to `timestamptz`). `None` when the
/// types cannot be compared, which the binder rules out.
pub(crate) fn compare(a: &Datum, b: &Datum) -> Option<Ordering> {
    use Datum::{Bool, Bytes, Date, Float, Int, Json, Numeric, Text, Timestamp, Uuid};
    Some(match (a, b) {
        (Bool(x), Bool(y)) => x.cmp(y),
        (Int(x), Int(y)) => x.cmp(y),
        (Float(x), Float(y)) => float_cmp(*x, *y),
        (Numeric(x), Numeric(y)) => x.cmp(y),
        (Int(x), Numeric(y)) => Decimal::from_i64(*x).cmp(y),
        (Numeric(x), Int(y)) => x.cmp(&Decimal::from_i64(*y)),
        (Int(x), Float(y)) => float_cmp(*x as f64, *y),
        (Float(x), Int(y)) => float_cmp(*x, *y as f64),
        (Numeric(x), Float(y)) => float_cmp(x.to_f64(), *y),
        (Float(x), Numeric(y)) => float_cmp(*x, y.to_f64()),
        (Text(x), Text(y)) | (Json(x), Json(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Bytes(x), Bytes(y)) => x.cmp(y),
        (Uuid(x), Uuid(y)) => x.cmp(y),
        (Date(x), Date(y)) => x.cmp(y),
        (Timestamp(x), Timestamp(y)) => x.cmp(y),
        (Date(x), Timestamp(y)) => date_to_micros(*x).cmp(y),
        (Timestamp(x), Date(y)) => x.cmp(&date_to_micros(*y)),
        _ => return None,
    })
}

/// PostgreSQL orders `NaN` above every number and equal to itself.
fn float_cmp(x: f64, y: f64) -> Ordering {
    match (x.is_nan(), y.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn civil_dates_round_trip_and_match_known_days() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        for z in (-800_000..800_000).step_by(997) {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
    }

    #[test]
    fn timestamps_parse_and_print() {
        let t = parse_timestamp("2024-02-29 13:45:10.5+00").unwrap();
        assert_eq!(format_timestamp(t), "2024-02-29 13:45:10.5+00");
        assert_eq!(parse_timestamp("2024-02-29T13:45:10Z"), Some(t - 500_000));
        assert_eq!(
            parse_timestamp("2024-02-29 15:45:10.5+02"),
            Some(t),
            "offsets are applied"
        );
        assert_eq!(parse_timestamp("2023-02-29"), None);
        assert_eq!(parse_timestamp("2024-13-01"), None);
        assert_eq!(parse_date("2024-02-29 10:00"), Some(19_782));
        assert_eq!(format_date(19_782), "2024-02-29");
    }

    #[test]
    fn text_forms_validate() {
        assert!(matches!(parse_text("x", SqlType::Integer), Err(e) if e.sqlstate() == "22P02"));
        assert!(
            matches!(parse_text("40000", SqlType::SmallInt), Err(e) if e.sqlstate() == "22003")
        );
        assert_eq!(
            parse_text(" 12 ", SqlType::Integer).unwrap(),
            Datum::Int(12)
        );
        assert_eq!(
            parse_text("\\xdead", SqlType::Bytea)
                .unwrap()
                .to_pg_text()
                .unwrap(),
            "\\xdead"
        );
        assert!(parse_text("\\xdea", SqlType::Bytea).is_err());
        let u = parse_text("A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11", SqlType::Uuid).unwrap();
        assert_eq!(
            u.to_pg_text().unwrap(),
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"
        );
        assert!(parse_text("{\"a\": [1, 2, {\"b\": null}]}", SqlType::Jsonb).is_ok());
        assert!(parse_text("{\"a\": }", SqlType::Jsonb).is_err());
        assert!(parse_text("[1,]", SqlType::Jsonb).is_err());
    }

    #[test]
    fn floats_print_like_postgres() {
        assert_eq!(format_float(1.5), "1.5");
        assert_eq!(format_float(1.0), "1");
        assert_eq!(format_float(1e20), "1e+20");
        assert_eq!(format_float(1.5e-7), "1.5e-07");
        assert_eq!(format_float(f64::NAN), "NaN");
    }

    #[test]
    fn comparison_promotes_numerics() {
        use Datum::{Float, Int, Numeric};
        let two = Decimal::parse("2.0").unwrap();
        assert_eq!(compare(&Int(2), &Numeric(two)), Some(Ordering::Equal));
        assert_eq!(compare(&Int(3), &Float(2.5)), Some(Ordering::Greater));
        assert_eq!(
            compare(&Float(f64::NAN), &Float(1.0)),
            Some(Ordering::Greater)
        );
        assert_eq!(compare(&Int(1), &Datum::Text("1".into())), None);
    }
}
