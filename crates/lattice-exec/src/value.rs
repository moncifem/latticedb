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

/// The SQLSTATE for text that failed to parse as a date or timestamp. PostgreSQL separates text
/// that is not shaped like a date (`22007`, invalid syntax) from text that is shaped like one but
/// names a day, month, year, hour, minute or second that does not exist (`22008`, field value out
/// of range), such as `2023-02-29` or `2024-01-01 25:00:00`.
fn datetime_failure_code(s: &str) -> &'static str {
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    let t = without_ad_era(s.trim());
    let (date, rest) = match t.find([' ', 'T']) {
        Some(at) => (
            t.get(..at).unwrap_or(""),
            t.get(at + 1..).unwrap_or("").trim(),
        ),
        None => (t, ""),
    };
    let fields: Vec<&str> = date.split('-').collect();
    if fields.len() != 3 || !fields.iter().all(|f| digits(f)) {
        return "22007";
    }
    // PostgreSQL reads every field before it checks ranges, so text after the date that is not a
    // clock (`2023-02-29 junk`) is a syntax error even when the date itself is out of range.
    let mut clock_out_of_range = false;
    if !rest.is_empty() {
        let Some((clock, _)) = split_offset(rest) else {
            return if offset_out_of_range(rest) {
                "22009"
            } else {
                "22007"
            };
        };
        fn whole(p: &str) -> &str {
            p.split_once('.').map_or(p, |(w, _)| w)
        }
        if clock.contains(['+', '-']) && offset_out_of_range(clock) {
            return "22009";
        }
        let parts: Vec<&str> = clock.split(':').collect();
        if !(2..=3).contains(&parts.len()) || !parts.iter().all(|p| digits(whole(p))) {
            return "22007";
        }
        let num = |p: Option<&&str>| p.and_then(|p| whole(p).parse::<i64>().ok()).unwrap_or(0);
        let (h, mi, sec) = (num(parts.first()), num(parts.get(1)), num(parts.get(2)));
        clock_out_of_range = h > 24 || mi > 59 || sec > 60 || (h == 24 && (mi > 0 || sec > 0));
    }
    if parse_date_part(date).is_none() || clock_out_of_range {
        "22008"
    } else {
        "22007"
    }
}

/// Parses a `date` (a timestamp's date part is accepted, as in PostgreSQL).
pub(crate) fn parse_date(s: &str) -> Option<i32> {
    let t = s.trim();
    let date = t.split([' ', 'T']).next()?;
    // PostgreSQL reads the time of day and ignores it, so what follows the date has to be a valid
    // time or the text is refused. `BC` (a year before 1 AD, outside the date range) and stray text
    // must not be read as the date in front of them.
    if date.len() < t.len() {
        parse_timestamp(without_ad_era(t))?;
    }
    i32::try_from(parse_date_part(date)?).ok()
}

/// The text before a trailing `AD` era (the default era, in any case), or the text itself. The era
/// is a word of its own, so it must follow whitespace: `10:00ad` is not a time followed by an era.
fn without_ad_era(t: &str) -> &str {
    let Some(at) = t.len().checked_sub(2) else {
        return t;
    };
    match (t.get(..at), t.get(at..)) {
        (Some(body), Some(era))
            if era.eq_ignore_ascii_case("ad") && body.ends_with(char::is_whitespace) =>
        {
            body.trim_end()
        }
        _ => t,
    }
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
        // Hour 24 is only the end of the day: 24:00:00, never 24:30.
        || (h == 24 && (mi > 0 || sec > 0))
        || !(0..60).contains(&mi)
        || !(0..=60).contains(&sec)
    {
        return None;
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // PostgreSQL reads the fraction as a double and rounds it to the nearest microsecond (ties to even,
    // on the double's value), so a seventh digit rounds instead of being refused, and `.9999995` carries
    // into the next second.
    let us: i64 = if frac.is_empty() {
        0
    } else {
        let seconds: f64 = format!("0.{frac}").parse().ok()?;
        let micros_f = (seconds * 1_000_000.0).round_ties_even();
        // At most 1,000,000 (a carry): the digits are a fraction below one.
        micros_f as i64
    };
    micros = micros.checked_add(h * 3_600_000_000 + mi * 60_000_000 + sec * 1_000_000 + us)?;
    micros = micros.checked_sub(offset_minutes.checked_mul(60_000_000)?)?;
    Some(micros)
}

/// The numeric fields of a trailing `+hh[:mm]` offset, or a colon-less one (`+h`, `+hh`, `+hmm`,
/// `+hhmm`, ...): hours and minutes as typed, the sign, and the text before the offset. `None` when
/// there is no such offset or it is not numeric.
fn offset_fields(s: &str) -> Option<(&str, i64, i64, i64)> {
    let at = s.rfind(['+', '-'])?;
    let (body, off) = s.split_at(at);
    let sign = if off.starts_with('-') { -1 } else { 1 };
    let off = off.get(1..)?;
    let (hh, mm): (i64, i64) = match off.split_once(':') {
        Some((a, b)) => (a.parse().ok()?, b.parse().ok()?),
        // PostgreSQL reads a colon-less offset of three or more digits as one number n, with hours
        // n / 100 and minutes n % 100: `+0530` and `+530` are 5:30, and `+05030` is 50 hours.
        None if off.len() >= 3 => {
            let n: i64 = off.parse().ok()?;
            (n / 100, n % 100)
        }
        None => (off.parse().ok()?, 0),
    };
    Some((body, sign, hh, mm))
}

/// Most PostgreSQL accepts in a zone offset: 15 hours 59 minutes either side of UTC.
const MAX_OFFSET_HOURS: i64 = 15;

fn split_offset(s: &str) -> Option<(&str, i64)> {
    if let Some(body) = s.strip_suffix(['Z', 'z']) {
        return Some((body.trim_end(), 0));
    }
    if s.rfind(['+', '-']).is_some() {
        let (body, sign, hh, mm) = offset_fields(s)?;
        // Larger or negative-looking fields are errors, and the arithmetic cannot overflow.
        if !(0..=MAX_OFFSET_HOURS).contains(&hh) || !(0..=59).contains(&mm) {
            return None;
        }
        let minutes = hh.checked_mul(60)?.checked_add(mm)?;
        return Some((body.trim_end(), sign * minutes));
    }
    Some((s, 0))
}

/// Whether the sign-led tail of `s` is made only of digits, colons, signs and dots, so that an offset
/// which failed to parse failed on its value (PostgreSQL's `22009`, invalid time zone displacement)
/// and not on its spelling (`22007`).
fn offset_out_of_range(s: &str) -> bool {
    let Some(at) = s.find(['+', '-']) else {
        return false;
    };
    let tail = s.get(at + 1..).unwrap_or("");
    tail.starts_with(|c: char| c.is_ascii_digit())
        && tail
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b':' | b'+' | b'-' | b'.'))
}

/// Days of a date as microseconds, for comparing a date with a timestamp.
pub(crate) fn date_to_micros(d: i32) -> Option<i64> {
    i64::from(d).checked_mul(MICROS_PER_DAY)
}

/// The day numbers of 0001-01-01 and 9999-12-31, the dates the subset accepts.
fn day_bounds() -> (i64, i64) {
    (days_from_civil(1, 1, 1), days_from_civil(9999, 12, 31))
}

/// Whether a stored day number is a date the subset can produce.
pub(crate) fn date_in_range(d: i32) -> bool {
    let (lo, hi) = day_bounds();
    (lo..=hi).contains(&i64::from(d))
}

/// Whether stored microseconds are a timestamp the subset can produce.
pub(crate) fn timestamp_in_range(t: i64) -> bool {
    let (lo, hi) = day_bounds();
    (lo..=hi).contains(&t.div_euclid(MICROS_PER_DAY))
}

pub(crate) fn micros_to_date(t: i64) -> Option<i32> {
    i32::try_from(t.div_euclid(MICROS_PER_DAY)).ok()
}

/// `uuid` input as PostgreSQL reads it: 32 hex digits, optionally in braces, with an optional hyphen
/// after any group of four digits except the last. White space is not skipped.
pub(crate) fn parse_uuid(s: &str) -> Option<u128> {
    let body = match s.strip_prefix('{') {
        Some(rest) => rest.strip_suffix('}')?,
        None => s,
    };
    let mut digits = body.bytes().peekable();
    let mut value = 0u128;
    for count in 1..=32u32 {
        let d = char::from(digits.next()?).to_digit(16)?;
        value = (value << 4) | u128::from(d);
        if count.is_multiple_of(4) && count < 32 && digits.peek() == Some(&b'-') {
            digits.next();
        }
    }
    digits.next().is_none().then_some(value)
}

/// `bytea` input as PostgreSQL reads it: the hex form `\\x4142` (pairs of digits, white space allowed
/// between pairs) or the escape form, where `\\\\` is a backslash and `\\ooo` an octal byte. The error is
/// the SQLSTATE: `22023` for a bad hex form, `22P02` for a bad escape.
fn parse_bytea(s: &str) -> Result<Vec<u8>, &'static str> {
    let bytes = s.as_bytes();
    if let Some(hex) = bytes.strip_prefix(b"\\x") {
        let mut out = Vec::with_capacity(hex.len() / 2);
        let mut pending: Option<u32> = None;
        for &c in hex {
            if matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
                if pending.is_some() {
                    return Err("22023");
                }
                continue;
            }
            let digit = char::from(c).to_digit(16).ok_or("22023")?;
            match pending.take() {
                Some(hi) => out.push(u8::try_from(hi * 16 + digit).map_err(|_| "22023")?),
                None => pending = Some(digit),
            }
        }
        return if pending.is_some() {
            Err("22023")
        } else {
            Ok(out)
        };
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&c) = bytes.get(i) {
        if c != b'\\' {
            out.push(c);
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'\\') => {
                out.push(b'\\');
                i += 2;
            }
            Some(d0 @ b'0'..=b'3') => {
                let octal = |k: usize| bytes.get(i + k).filter(|d| (b'0'..=b'7').contains(d));
                let (d1, d2) = (octal(2).ok_or("22P02")?, octal(3).ok_or("22P02")?);
                let v = u32::from(d0 - b'0') * 64 + u32::from(d1 - b'0') * 8 + u32::from(d2 - b'0');
                out.push(u8::try_from(v).map_err(|_| "22P02")?);
                i += 4;
            }
            _ => return Err("22P02"),
        }
    }
    Ok(out)
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

/// Checks that `s` is one JSON value (RFC 8259), nested at most 64 deep. On failure the SQLSTATE PostgreSQL gives: `22P05` for a NUL escape,
/// which it cannot store in text, and `22P02` for any other fault, including an unpaired surrogate escape.
pub(crate) fn json_check(s: &str) -> Result<(), &'static str> {
    let b = s.as_bytes();
    let mut p = JsonScan {
        b,
        i: 0,
        nul_escape: false,
    };
    p.ws();
    let ok = p.value(0) && {
        p.ws();
        p.i == b.len()
    };
    if ok {
        Ok(())
    } else if p.nul_escape {
        Err("22P05")
    } else {
        Err("22P02")
    }
}

struct JsonScan<'a> {
    b: &'a [u8],
    i: usize,
    /// A `\u0000` escape was met (PostgreSQL refuses it with its own SQLSTATE).
    nul_escape: bool,
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
    /// Four hex digits as a UTF-16 code unit, advancing past them.
    fn hex4(&mut self) -> Option<u32> {
        let mut unit = 0u32;
        for _ in 0..4 {
            let digit = char::from(self.peek()?).to_digit(16)?;
            unit = unit * 16 + digit;
            self.i += 1;
        }
        Some(unit)
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
                            let Some(unit) = self.hex4() else {
                                return false;
                            };
                            match unit {
                                0 => {
                                    self.nul_escape = true;
                                    return false;
                                }
                                // A high surrogate must be followed by an escaped low surrogate.
                                0xD800..=0xDBFF => {
                                    if self.peek() != Some(b'\\')
                                        || self.b.get(self.i + 1) != Some(&b'u')
                                    {
                                        return false;
                                    }
                                    self.i += 2;
                                    if !matches!(self.hex4(), Some(0xDC00..=0xDFFF)) {
                                        return false;
                                    }
                                }
                                // A low surrogate on its own is not a character.
                                0xDC00..=0xDFFF => return false,
                                _ => {}
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

/// `s` without the single underscores PostgreSQL 16 accepts between digits of an integer or numeric
/// (`1_000`). An underscore anywhere else is a syntax error, so this is `None`.
fn without_digit_separators(s: &str) -> Option<std::borrow::Cow<'_, str>> {
    if !s.contains('_') {
        return Some(std::borrow::Cow::Borrowed(s));
    }
    let b = s.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        if c == b'_' {
            let digit_at =
                |j: Option<usize>| j.and_then(|j| b.get(j)).is_some_and(u8::is_ascii_digit);
            if !digit_at(i.checked_sub(1)) || !digit_at(Some(i + 1)) {
                return None;
            }
        }
    }
    Some(std::borrow::Cow::Owned(s.replace('_', "")))
}

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
            let t = without_digit_separators(s.trim()).ok_or_else(|| invalid(ty, s))?;
            let t = t.as_ref();
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
        SqlType::Numeric => Datum::Numeric(
            Decimal::parse(&without_digit_separators(s).ok_or_else(|| invalid(ty, s))?).map_err(
                |e| match e {
                    NumError::Syntax => invalid(ty, s),
                    other => other.into(),
                },
            )?,
        ),
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
        SqlType::Bytea => Datum::Bytes(parse_bytea(s).map_err(|code| {
            ExecError::eval(code, format!("invalid input syntax for type {ty}: \"{s}\""))
        })?),
        SqlType::Uuid => Datum::Uuid(parse_uuid(s).ok_or_else(|| invalid(ty, s))?),
        SqlType::Date => {
            let d = parse_date(s).ok_or_else(|| {
                ExecError::eval(
                    datetime_failure_code(s),
                    format!("invalid input syntax for type date: \"{s}\""),
                )
            })?;
            if !date_in_range(d) {
                return Err(ExecError::eval("22008", "date out of range"));
            }
            Datum::Date(d)
        }
        SqlType::Timestamptz => {
            let t = parse_timestamp(s).ok_or_else(|| {
                ExecError::eval(
                    datetime_failure_code(s),
                    format!("invalid input syntax for type timestamp with time zone: \"{s}\""),
                )
            })?;
            // A zone offset can move a timestamp across the first or last day of the range.
            if !timestamp_in_range(t) {
                return Err(ExecError::eval("22008", "timestamp out of range"));
            }
            Datum::Timestamp(t)
        }
        SqlType::Jsonb => {
            if let Err(code) = json_check(s) {
                return Err(ExecError::eval(
                    code,
                    format!("invalid input syntax for type json: \"{s}\""),
                ));
            }
            // The canonical text, as PostgreSQL prints a jsonb. A number too long to write out is
            // refused as out of range.
            match crate::jsonb::canonical(s) {
                Some(text) => Datum::Json(text),
                None => {
                    return Err(ExecError::eval(
                        "22003",
                        "jsonb number is outside the range this server writes out",
                    ));
                }
            }
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
        (Date(x), Timestamp(y)) => date_to_micros(*x).unwrap_or(i64::MAX).cmp(y),
        (Timestamp(x), Date(y)) => x.cmp(&date_to_micros(*y).unwrap_or(i64::MAX)),
        _ => return None,
    })
}

/// A total order over any two non-null values, for sorting: [`compare`] where the types compare,
/// and the kind of value otherwise, so a sort never depends on an inconsistent comparison.
pub(crate) fn total_order(a: &Datum, b: &Datum) -> Ordering {
    fn rank(d: &Datum) -> u8 {
        match d {
            Datum::Null => 0,
            Datum::Bool(_) => 1,
            Datum::Int(_) | Datum::Float(_) | Datum::Numeric(_) => 2,
            Datum::Text(_) => 3,
            Datum::Bytes(_) => 4,
            Datum::Uuid(_) => 5,
            Datum::Date(_) | Datum::Timestamp(_) => 6,
            Datum::Json(_) => 7,
        }
    }
    compare(a, b).unwrap_or_else(|| rank(a).cmp(&rank(b)))
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
    fn dates_outside_the_subset_do_not_overflow_when_widened() {
        assert_eq!(date_to_micros(0), Some(0));
        assert_eq!(date_to_micros(i32::MAX), None);
        assert_eq!(date_to_micros(i32::MIN), None);
        assert!(date_in_range(0) && date_in_range(19_782));
        assert!(!date_in_range(i32::MAX) && !date_in_range(-800_000));
        assert!(timestamp_in_range(0));
        assert!(!timestamp_in_range(i64::MAX) && !timestamp_in_range(i64::MIN));
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
