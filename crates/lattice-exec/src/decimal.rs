//! Exact decimal numbers for `numeric`: a signed 128-bit mantissa and a decimal scale.
//!
//! The subset allows up to 38 digits, which fit in an `i128`. Every operation is checked and reports
//! [`NumError`] instead of wrapping; none panics.

use std::cmp::Ordering;
use std::fmt;

/// Largest scale kept.
pub(crate) const MAX_SCALE: u32 = 38;
/// Significant digits PostgreSQL aims for in a quotient (`NUMERIC_MIN_SIG_DIGITS`).
const MIN_SIG_DIGITS: i64 = 16;
/// Decimal digits per base-10000 digit group, PostgreSQL's storage unit.
const GROUP_DIGITS: i64 = 4;

/// Why a decimal operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NumError {
    /// The result does not fit (SQLSTATE `22003`).
    Overflow,
    /// Division by zero (`22012`).
    DivisionByZero,
    /// The text is not a number (`22P02`).
    Syntax,
}

/// An exact decimal: `mantissa / 10^scale`.
#[derive(Clone, Copy, Debug)]
pub struct Decimal {
    mantissa: i128,
    scale: u32,
}

fn pow10(n: u32) -> Option<i128> {
    10i128.checked_pow(n)
}

/// `i128::MIN` is never a mantissa: it has no positive counterpart, so negating, taking the absolute
/// value or dividing it by -1 would overflow, and its text does not parse back.
fn valid(mantissa: i128) -> Result<i128, NumError> {
    if mantissa == i128::MIN {
        Err(NumError::Overflow)
    } else {
        Ok(mantissa)
    }
}

impl Decimal {
    /// Builds a decimal; the scale is limited to 38 digits.
    pub(crate) fn new(mantissa: i128, scale: u32) -> Result<Self, NumError> {
        if scale > MAX_SCALE {
            return Err(NumError::Overflow);
        }
        Ok(Decimal {
            mantissa: valid(mantissa)?,
            scale,
        })
    }

    pub(crate) fn from_i64(v: i64) -> Self {
        Decimal {
            mantissa: i128::from(v),
            scale: 0,
        }
    }

    /// Parses `[+-]digits[.digits][e[+-]digits]`, surrounding blanks ignored.
    pub(crate) fn parse(text: &str) -> Result<Self, NumError> {
        let t = text.trim();
        let (neg, t) = match t.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, t.strip_prefix('+').unwrap_or(t)),
        };
        let (body, exp) = match t.find(['e', 'E']) {
            Some(at) => {
                let (b, e) = t.split_at(at);
                let e = e.get(1..).unwrap_or("");
                let exp: i32 = e.parse().map_err(|_| NumError::Syntax)?;
                (b, exp)
            }
            None => (t, 0),
        };
        let (int, frac) = match body.split_once('.') {
            Some((a, b)) => (a, b),
            None => (body, ""),
        };
        if int.is_empty() && frac.is_empty() {
            return Err(NumError::Syntax);
        }
        if !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return Err(NumError::Syntax);
        }
        let mut mantissa: i128 = 0;
        let mut dropped_round_up = false;
        let mut scale: i64 = 0;
        for (i, b) in int.bytes().chain(frac.bytes()).enumerate() {
            let d = i128::from(b - b'0');
            let in_frac = i >= int.len();
            if in_frac && scale >= i64::from(MAX_SCALE) {
                // Extra fractional digits beyond the scale limit: round half away from zero.
                if scale == i64::from(MAX_SCALE) && d >= 5 && !dropped_round_up {
                    dropped_round_up = true;
                }
                scale = i64::from(MAX_SCALE) + 1;
                continue;
            }
            mantissa = mantissa
                .checked_mul(10)
                .and_then(|m| m.checked_add(d))
                .ok_or(NumError::Overflow)?;
            if in_frac {
                scale += 1;
            }
        }
        if scale > i64::from(MAX_SCALE) {
            scale = i64::from(MAX_SCALE);
        }
        if dropped_round_up {
            mantissa = mantissa.checked_add(1).ok_or(NumError::Overflow)?;
        }
        // Zero is zero at any exponent; do not walk a huge one digit by digit.
        if mantissa == 0 {
            let scale = (scale - i64::from(exp)).clamp(0, i64::from(MAX_SCALE));
            return Ok(Decimal {
                mantissa: 0,
                scale: u32::try_from(scale).map_err(|_| NumError::Overflow)?,
            });
        }
        // Apply the exponent by moving the decimal point.
        let mut scale = scale - i64::from(exp);
        while scale < 0 {
            mantissa = mantissa.checked_mul(10).ok_or(NumError::Overflow)?;
            scale += 1;
        }
        let mut d = Decimal {
            mantissa,
            scale: u32::try_from(scale).map_err(|_| NumError::Overflow)?,
        };
        while d.scale > MAX_SCALE {
            d = d.round_to(MAX_SCALE)?;
        }
        if neg {
            d.mantissa = -d.mantissa;
        }
        valid(d.mantissa)?;
        Ok(d)
    }

    fn rescale_up(self, scale: u32) -> Result<Self, NumError> {
        if scale <= self.scale {
            return Ok(self);
        }
        let f = pow10(scale - self.scale).ok_or(NumError::Overflow)?;
        Ok(Decimal {
            mantissa: valid(self.mantissa.checked_mul(f).ok_or(NumError::Overflow)?)?,
            scale,
        })
    }

    /// Rounds half away from zero to `scale` fractional digits (no change when already finer).
    pub(crate) fn round_to(self, scale: u32) -> Result<Self, NumError> {
        if scale >= self.scale {
            return Ok(self);
        }
        // Dropping more than 38 digits leaves less than half a unit of the last place: zero.
        let Some(f) = pow10(self.scale - scale) else {
            return Ok(Decimal { mantissa: 0, scale });
        };
        let q = self.mantissa.checked_div(f).ok_or(NumError::Overflow)?;
        let r = self.mantissa.checked_rem(f).ok_or(NumError::Overflow)?;
        let q = if r.unsigned_abs() * 2 >= f.unsigned_abs() {
            q + if self.mantissa < 0 { -1 } else { 1 }
        } else {
            q
        };
        Ok(Decimal {
            mantissa: valid(q)?,
            scale,
        })
    }

    pub(crate) fn neg(self) -> Result<Self, NumError> {
        Ok(Decimal {
            mantissa: self.mantissa.checked_neg().ok_or(NumError::Overflow)?,
            scale: self.scale,
        })
    }

    pub(crate) fn abs(self) -> Result<Self, NumError> {
        if self.mantissa < 0 {
            self.neg()
        } else {
            Ok(self)
        }
    }

    pub(crate) fn add(self, o: Self) -> Result<Self, NumError> {
        let s = self.scale.max(o.scale);
        let (a, b) = (self.rescale_up(s)?, o.rescale_up(s)?);
        Ok(Decimal {
            mantissa: valid(
                a.mantissa
                    .checked_add(b.mantissa)
                    .ok_or(NumError::Overflow)?,
            )?,
            scale: s,
        })
    }

    pub(crate) fn sub(self, o: Self) -> Result<Self, NumError> {
        self.add(o.neg()?)
    }

    pub(crate) fn mul(self, o: Self) -> Result<Self, NumError> {
        let m = self
            .mantissa
            .checked_mul(o.mantissa)
            .ok_or(NumError::Overflow)?;
        let mut d = Decimal {
            mantissa: valid(m)?,
            scale: self.scale + o.scale,
        };
        while d.scale > MAX_SCALE {
            d = d.round_to(MAX_SCALE)?;
        }
        Ok(d)
    }

    /// Weight of the first base-10000 digit group and that group's value, as PostgreSQL stores them
    /// (a group counts the four decimal places at its weight, zero padded): `7.5` has weight 0 and
    /// first group 7, `100` weight 0 and group 100, `0.1` weight -1 and group 1000, `0.0012` weight -1
    /// and group 12, `12345` weight 1 and group 1. Zero has weight 0 and group 0.
    fn weight_and_first_group(self) -> (i64, i128) {
        if self.mantissa == 0 {
            return (0, 0);
        }
        let magnitude = self.mantissa.unsigned_abs();
        let digits = i64::from(magnitude.ilog10()) + 1;
        // Exponent of the leading decimal digit: 7.5 -> 0, 0.0012 -> -3, 100 -> 2.
        let lead = digits - 1 - i64::from(self.scale);
        let weight = lead.div_euclid(GROUP_DIGITS);
        // The group is the value divided by 10^(4 * weight), truncated.
        let t = i64::from(self.scale) + GROUP_DIGITS * weight;
        let group = if t >= 0 {
            magnitude / 10u128.pow(u32::try_from(t).unwrap_or(0))
        } else {
            magnitude.saturating_mul(10u128.pow(u32::try_from(-t).unwrap_or(0)))
        };
        (weight, i128::try_from(group).unwrap_or(0))
    }

    /// The scale of a quotient, chosen as PostgreSQL does (`select_div_scale`): about 16 significant
    /// digits, never fewer fractional digits than either operand, at most 38 here.
    fn division_scale(self, o: Self) -> u32 {
        let (w1, g1) = self.weight_and_first_group();
        let (w2, g2) = o.weight_and_first_group();
        let mut qweight = w1 - w2;
        if g1 <= g2 {
            qweight -= 1;
        }
        let wanted = (MIN_SIG_DIGITS - qweight * GROUP_DIGITS)
            .max(i64::from(self.scale))
            .max(i64::from(o.scale))
            .clamp(0, i64::from(MAX_SCALE));
        u32::try_from(wanted).unwrap_or(MAX_SCALE)
    }

    /// Division with the result scale PostgreSQL would choose, limited to 38 fractional digits.
    pub(crate) fn div(self, o: Self) -> Result<Self, NumError> {
        if o.mantissa == 0 {
            return Err(NumError::DivisionByZero);
        }
        let mut r = self.division_scale(o);
        loop {
            // quotient = a / 10^sa / (b / 10^sb) = a * 10^(r + sb - sa) / b / 10^r
            let e = i64::from(r) + i64::from(o.scale) - i64::from(self.scale);
            let attempt = u32::try_from(e).ok().and_then(|e| {
                let f = pow10(e)?;
                let n = self.mantissa.checked_mul(f)?;
                let q = n.checked_div(o.mantissa)?;
                let rem = n.checked_rem(o.mantissa)?;
                // Round half away from zero: |rem| * 2 >= |divisor|, written without overflow.
                let half_up = rem.unsigned_abs() >= o.mantissa.unsigned_abs().div_ceil(2);
                if half_up {
                    q.checked_add(if (n < 0) != (o.mantissa < 0) { -1 } else { 1 })
                } else {
                    Some(q)
                }
            });
            match attempt {
                Some(q) => {
                    return Ok(Decimal {
                        mantissa: valid(q)?,
                        scale: r,
                    });
                }
                None if r > 0 => r -= 1,
                None => return Err(NumError::Overflow),
            }
        }
    }

    pub(crate) fn rem(self, o: Self) -> Result<Self, NumError> {
        if o.mantissa == 0 {
            return Err(NumError::DivisionByZero);
        }
        let s = self.scale.max(o.scale);
        let (a, b) = (self.rescale_up(s)?, o.rescale_up(s)?);
        Ok(Decimal {
            mantissa: valid(
                a.mantissa
                    .checked_rem(b.mantissa)
                    .ok_or(NumError::Overflow)?,
            )?,
            scale: s,
        })
    }

    /// The nearest integer, halves away from zero.
    pub(crate) fn to_i128_round(self) -> Result<i128, NumError> {
        Ok(self.round_to(0)?.mantissa)
    }

    pub(crate) fn to_f64(self) -> f64 {
        // Via text so the result is the correctly rounded double.
        self.to_string().parse().unwrap_or(f64::NAN)
    }

    pub(crate) fn from_f64(v: f64) -> Result<Self, NumError> {
        if !v.is_finite() {
            return Err(NumError::Overflow);
        }
        Decimal::parse(&format!("{v:e}"))
    }
}

impl PartialEq for Decimal {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Decimal {}

impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Decimal {
    fn cmp(&self, other: &Self) -> Ordering {
        let s = self.scale.max(other.scale);
        match (self.rescale_up(s), other.rescale_up(s)) {
            (Ok(a), Ok(b)) => a.mantissa.cmp(&b.mantissa),
            // The side that overflowed when scaled up is the larger in magnitude.
            (Err(_), Ok(_)) => {
                if self.mantissa < 0 {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (Ok(_), Err(_)) => {
                if other.mantissa < 0 {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (Err(_), Err(_)) => Ordering::Equal,
        }
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let neg = self.mantissa < 0;
        let digits = self.mantissa.unsigned_abs().to_string();
        let scale = self.scale as usize;
        let mut out = String::new();
        if neg {
            out.push('-');
        }
        if scale == 0 {
            out.push_str(&digits);
        } else if digits.len() > scale {
            let (a, b) = digits.split_at(digits.len() - scale);
            out.push_str(a);
            out.push('.');
            out.push_str(b);
        } else {
            out.push_str("0.");
            for _ in 0..scale - digits.len() {
                out.push('0');
            }
            out.push_str(&digits);
        }
        f.write_str(&out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::parse(s).unwrap()
    }

    #[test]
    fn parse_and_print() {
        assert_eq!(d("1.50").to_string(), "1.50");
        assert_eq!(d("-0.05").to_string(), "-0.05");
        assert_eq!(d("1e3").to_string(), "1000");
        assert_eq!(d("12.5e-1").to_string(), "1.25");
        assert_eq!(d(".5").to_string(), "0.5");
        assert_eq!(d("5.").to_string(), "5");
        assert_eq!(Decimal::parse("1.2.3"), Err(NumError::Syntax));
        assert_eq!(Decimal::parse(""), Err(NumError::Syntax));
        assert_eq!(Decimal::parse("."), Err(NumError::Syntax));
        assert_eq!(Decimal::parse("abc"), Err(NumError::Syntax));
        assert_eq!(Decimal::parse(&"9".repeat(40)), Err(NumError::Overflow));
    }

    #[test]
    fn arithmetic_is_exact_and_checked() {
        assert_eq!(d("0.1").add(d("0.2")).unwrap(), d("0.3"));
        assert_eq!(d("1.5").mul(d("2")).unwrap().to_string(), "3.0");
        assert_eq!(
            d("1").div(d("4")).unwrap().to_string(),
            "0.25000000000000000000"
        );
        assert_eq!(
            d("-1").div(d("3")).unwrap().to_string(),
            "-0.33333333333333333333"
        );
        assert_eq!(
            d("2").div(d("3")).unwrap().to_string(),
            "0.66666666666666666667"
        );
        assert_eq!(d("1").div(d("0")), Err(NumError::DivisionByZero));
        assert_eq!(d("7").rem(d("0")), Err(NumError::DivisionByZero));
        assert_eq!(d("7").rem(d("4")).unwrap(), d("3"));
        let big = d(&"9".repeat(38));
        assert_eq!(big.add(big), Err(NumError::Overflow));
        assert_eq!(big.mul(d("10")), Err(NumError::Overflow));
    }

    #[test]
    fn rounding_is_half_away_from_zero() {
        assert_eq!(d("2.5").to_i128_round().unwrap(), 3);
        assert_eq!(d("-2.5").to_i128_round().unwrap(), -3);
        assert_eq!(d("2.4").to_i128_round().unwrap(), 2);
        assert_eq!(d("1.005").round_to(2).unwrap().to_string(), "1.01");
        assert_eq!(d("-1.005").round_to(2).unwrap().to_string(), "-1.01");
    }

    #[test]
    fn comparison_ignores_scale_and_survives_overflow() {
        assert_eq!(d("1.0"), d("1.000"));
        assert!(d("1.1") > d("1.09"));
        let big = d(&"9".repeat(38));
        let tiny = d("0.00000000000000000000000000000000000001");
        assert!(big > tiny);
        assert!(big.neg().unwrap() < tiny);
    }

    #[test]
    fn the_extremes_are_errors_not_panics() {
        // i128::MIN is not a mantissa, so nothing can reach the operations that overflow on it.
        assert_eq!(Decimal::new(i128::MIN, 0), Err(NumError::Overflow));
        let max = Decimal::new(i128::MAX, 0).unwrap();
        assert_eq!(
            max.div(Decimal::from_i64(-1)).unwrap().to_string(),
            format!("-{}", i128::MAX)
        );
        assert_eq!(max.neg().unwrap().neg().unwrap(), max);
        let min = max.neg().unwrap();
        assert_eq!(min.abs().unwrap(), max);
        assert_eq!(min.rem(Decimal::from_i64(-1)).unwrap().to_string(), "0");
        // The negation of the largest value is the smallest the type keeps, and one more is refused.
        assert_eq!(min.sub(Decimal::from_i64(1)), Err(NumError::Overflow));
        assert_eq!(
            Decimal::parse("-170141183460469231731687303715884105728"),
            Err(NumError::Overflow)
        );
        assert_eq!(Decimal::parse(&i128::MAX.to_string()).unwrap(), max);
        assert_eq!(Decimal::parse(&format!("-{}", i128::MAX)).unwrap(), min);
    }

    #[test]
    fn a_zero_with_a_huge_exponent_returns_at_once() {
        assert_eq!(Decimal::parse("0e2147483647").unwrap().to_string(), "0");
        assert_eq!(
            Decimal::parse("0e-2147483648").unwrap().to_string(),
            "0.00000000000000000000000000000000000000"
        );
        assert_eq!(Decimal::parse("0.000e2147483647").unwrap().to_string(), "0");
        // Non-zero values with extreme exponents are errors or zero, quickly.
        assert_eq!(Decimal::parse("1e2147483647"), Err(NumError::Overflow));
        assert_eq!(
            Decimal::parse("1e-2147483648").unwrap().to_string(),
            "0.00000000000000000000000000000000000000"
        );
    }

    #[test]
    fn tiny_values_round_to_zero_instead_of_overflowing() {
        let tiny = Decimal::parse("1e-100").unwrap();
        assert_eq!(tiny.round_to(2).unwrap().to_string(), "0.00");
        assert_eq!(
            Decimal::parse("0.5e-39")
                .unwrap()
                .round_to(0)
                .unwrap()
                .to_string(),
            "0"
        );
    }

    mod adversarial {
        use super::*;
        use proptest::prelude::*;

        fn mantissa() -> impl Strategy<Value = i128> {
            prop_oneof![
                any::<i128>(),
                Just(i128::MAX),
                Just(i128::MIN),
                Just(i128::MIN + 1),
                Just(0),
                Just(-1),
                Just(1),
                -1000i128..1000,
            ]
        }

        fn decimal() -> impl Strategy<Value = Decimal> {
            (mantissa(), 0u32..=MAX_SCALE + 2)
                .prop_filter_map("valid", |(m, s)| Decimal::new(m, s).ok())
        }

        proptest! {
            /// Every operation on any pair, including the extremes, returns a value or an error, and
            /// whatever it returns prints and parses back to the same number.
            #[test]
            fn operations_never_panic_and_results_round_trip(a in decimal(), b in decimal(), digits in 0u32..40) {
                let results = [a.add(b), a.sub(b), a.mul(b), a.div(b), a.rem(b), a.neg(), a.abs(), a.round_to(digits)];
                for r in results.into_iter().flatten() {
                    prop_assert_eq!(Decimal::parse(&r.to_string()), Ok(r));
                }
                let _ = a.to_i128_round();
                let _ = a.cmp(&b);
                let _ = a.to_f64();
            }

            #[test]
            fn parse_never_panics_or_hangs(text in "[+-]?[0-9]{0,45}(\\.[0-9]{0,45})?([eE][+-]?[0-9]{1,10})?") {
                let _ = Decimal::parse(&text);
            }
        }
    }
}
