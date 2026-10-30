//! The canonical text of a `jsonb` value, as PostgreSQL prints it: object keys ordered by length and
//! then by bytes, a repeated key keeping its last value, `": "` and `", "` separators, numbers written
//! out without an exponent, and strings with only the escapes JSON requires. The input has already been
//! checked by `json_check`, so a fault here means the checker and this reader disagree.

enum Node {
    Null,
    Bool(bool),
    Number(String),
    Text(String),
    Array(Vec<Node>),
    Object(Vec<(String, Node)>),
}

/// The canonical text of one JSON value, or `None` when `s` is not one (or a number does not fit).
pub(crate) fn canonical(s: &str) -> Option<String> {
    let mut r = Reader {
        b: s.as_bytes(),
        i: 0,
    };
    r.ws();
    let node = r.value()?;
    r.ws();
    if r.i != r.b.len() {
        return None;
    }
    let mut out = String::with_capacity(s.len());
    write(&node, &mut out);
    Some(out)
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl Reader<'_> {
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

    fn value(&mut self) -> Option<Node> {
        match self.peek()? {
            b'n' => self.lit("null").then_some(Node::Null),
            b't' => self.lit("true").then_some(Node::Bool(true)),
            b'f' => self.lit("false").then_some(Node::Bool(false)),
            b'"' => self.string().map(Node::Text),
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                    return Some(Node::Array(items));
                }
                loop {
                    self.ws();
                    items.push(self.value()?);
                    self.ws();
                    match self.peek()? {
                        b',' => self.i += 1,
                        b']' => {
                            self.i += 1;
                            return Some(Node::Array(items));
                        }
                        _ => return None,
                    }
                }
            }
            b'{' => {
                self.i += 1;
                let mut members: Vec<(String, Node)> = Vec::new();
                self.ws();
                if self.peek() == Some(b'}') {
                    self.i += 1;
                    return Some(Node::Object(members));
                }
                loop {
                    self.ws();
                    let key = self.string()?;
                    self.ws();
                    if self.peek()? != b':' {
                        return None;
                    }
                    self.i += 1;
                    self.ws();
                    let value = self.value()?;
                    // The last value of a repeated key wins.
                    match members.iter_mut().find(|(k, _)| *k == key) {
                        Some(slot) => slot.1 = value,
                        None => members.push((key, value)),
                    }
                    self.ws();
                    match self.peek()? {
                        b',' => self.i += 1,
                        b'}' => {
                            self.i += 1;
                            members.sort_by(|a, b| {
                                a.0.len()
                                    .cmp(&b.0.len())
                                    .then_with(|| a.0.as_bytes().cmp(b.0.as_bytes()))
                            });
                            return Some(Node::Object(members));
                        }
                        _ => return None,
                    }
                }
            }
            _ => self.number().map(Node::Number),
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.b.get(self.i..self.i + 4)?;
        let mut v = 0u32;
        for d in digits {
            v = v * 16 + char::from(*d).to_digit(16)?;
        }
        self.i += 4;
        Some(v)
    }

    fn string(&mut self) -> Option<String> {
        if self.peek()? != b'"' {
            return None;
        }
        self.i += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = self.peek()?;
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let e = self.peek()?;
                    self.i += 1;
                    match e {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let mut unit = self.hex4()?;
                            if (0xD800..0xDC00).contains(&unit) {
                                if !self.lit("\\u") {
                                    return None;
                                }
                                let low = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&low) {
                                    return None;
                                }
                                unit = 0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00);
                            }
                            let ch = char::from_u32(unit)?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return None,
                    }
                }
                other => out.push(other),
            }
        }
    }

    /// A JSON number as the text of the `numeric` PostgreSQL stores: no exponent, the fraction digits
    /// the input spelled (after the exponent has moved the point), and no sign on zero.
    fn number(&mut self) -> Option<String> {
        let start = self.i;
        let negative = self.peek() == Some(b'-');
        if negative {
            self.i += 1;
        }
        let digits_from = self.i;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        let int_part = std::str::from_utf8(self.b.get(digits_from..self.i)?).ok()?;
        if int_part.is_empty() {
            return None;
        }
        let mut frac_part = "";
        if self.peek() == Some(b'.') {
            self.i += 1;
            let from = self.i;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.i += 1;
            }
            frac_part = std::str::from_utf8(self.b.get(from..self.i)?).ok()?;
            if frac_part.is_empty() {
                return None;
            }
        }
        let mut exponent: i64 = 0;
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            let sign = match self.peek() {
                Some(b'-') => {
                    self.i += 1;
                    -1
                }
                Some(b'+') => {
                    self.i += 1;
                    1
                }
                _ => 1,
            };
            let from = self.i;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.i += 1;
            }
            let text = std::str::from_utf8(self.b.get(from..self.i)?).ok()?;
            if text.is_empty() || text.len() > 6 {
                return None;
            }
            exponent = sign * text.parse::<i64>().ok()?;
        }
        if self.i == start {
            return None;
        }
        plain_decimal(negative, int_part, frac_part, exponent)
    }
}

/// Largest number of digits written for one number; PostgreSQL's numeric holds far more, but a
/// bound keeps `1e999999` from building a megabyte string.
const MAX_NUMBER_DIGITS: i64 = 1000;

fn plain_decimal(negative: bool, int_part: &str, frac_part: &str, exponent: i64) -> Option<String> {
    // digits = int_part ++ frac_part, with the point after `int_part.len() + exponent` digits.
    let mut digits: Vec<u8> = int_part.bytes().chain(frac_part.bytes()).collect();
    let mut point = i64::try_from(int_part.len()).ok()? + exponent;
    // Scale is the number of digits after the point, never negative.
    if point > MAX_NUMBER_DIGITS || -point > MAX_NUMBER_DIGITS {
        return None;
    }
    if point < 0 {
        let mut zeros = vec![b'0'; usize::try_from(-point).ok()?];
        zeros.extend_from_slice(&digits);
        digits = zeros;
        point = 0;
    }
    let point = usize::try_from(point).ok()?;
    if point > digits.len() {
        digits.resize(point, b'0');
    }
    let (whole, fraction) = digits.split_at(point);
    let whole: &[u8] = {
        let first = whole.iter().position(|d| *d != b'0').unwrap_or(whole.len());
        whole.get(first..).unwrap_or_default()
    };
    let mut out = String::new();
    let is_zero = digits.iter().all(|d| *d == b'0');
    if negative && !is_zero {
        out.push('-');
    }
    if whole.is_empty() {
        out.push('0');
    } else {
        out.push_str(std::str::from_utf8(whole).ok()?);
    }
    if !fraction.is_empty() {
        out.push('.');
        out.push_str(std::str::from_utf8(fraction).ok()?);
    }
    Some(out)
}

fn write(node: &Node, out: &mut String) {
    match node {
        Node::Null => out.push_str("null"),
        Node::Bool(true) => out.push_str("true"),
        Node::Bool(false) => out.push_str("false"),
        Node::Number(n) => out.push_str(n),
        Node::Text(s) => write_string(s, out),
        Node::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write(item, out);
            }
            out.push(']');
        }
        Node::Object(members) => {
            out.push('{');
            for (i, (k, v)) in members.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_string(k, out);
                out.push_str(": ");
                write(v, out);
            }
            out.push('}');
        }
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
