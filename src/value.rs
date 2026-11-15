//! Scalar types stored in a row.

use std::fmt;

/// A column type.
///
/// Integer SQL names (`INT`, `INTEGER`, `BIGINT`, and the smaller integer
/// aliases) all use this one 64-bit type. The original name is not retained,
/// so it is not a storage constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// Signed 64-bit integer.
    Integer,
    /// Unicode text, stored as a Rust `String`.
    Text,
    /// Boolean.
    Boolean,
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Integer => "INTEGER",
            Self::Text => "TEXT",
            Self::Boolean => "BOOLEAN",
        })
    }
}

/// One scalar cell.
///
/// `Null` is a value in every column. It has no runtime type of its own: a
/// null literal may be stored in an integer, text, or boolean column, and a
/// stored null still occupies that column.
///
/// [`Value::Ratio`] is a query result from `AVG`. It is not a column type.
/// The record codec refuses to write it, and `INSERT` / `UPDATE` reject it
/// before a row is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// SQL `NULL`.
    Null,
    /// Signed 64-bit integer.
    Integer(i64),
    /// Text.
    Text(String),
    /// Boolean.
    Boolean(bool),
    /// Reduced query-only ratio. The denominator is positive.
    Ratio { numerator: i128, denominator: i64 },
}

impl Value {
    /// The runtime type of a non-null value.
    ///
    /// `NULL` returns [`None`] because a null literal is compatible with every
    /// column type. A column reference still has the column's declared type
    /// even when the stored cell is null.
    #[must_use]
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Self::Null | Self::Ratio { .. } => None,
            Self::Integer(_) => Some(DataType::Integer),
            Self::Text(_) => Some(DataType::Text),
            Self::Boolean(_) => Some(DataType::Boolean),
        }
    }

    /// A reduced ratio with a positive denominator when that form fits.
    ///
    /// A zero denominator is rejected. A negative denominator flips onto the
    /// numerator. `+2^127` and a reduced denominator of `2^63` do not fit in
    /// this pair; those inputs keep their original bits, and display and
    /// comparison still use the exact value. `AVG` does not build either one.
    pub(crate) fn ratio(numerator: i128, denominator: i64) -> Self {
        assert_nonzero_denominator(denominator);
        let (numerator, denominator) = normalize_ratio(numerator, denominator);
        Self::Ratio {
            numerator,
            denominator,
        }
    }

    /// Refuse a query-only ratio before it can be stored.
    pub(crate) fn ensure_persistent(&self) -> Result<(), crate::error::DbError> {
        match self {
            Self::Ratio { .. } => Err(crate::error::DbError::Internal(
                "ratio values are not stored".to_string(),
            )),
            Self::Null | Self::Integer(_) | Self::Text(_) | Self::Boolean(_) => Ok(()),
        }
    }

    /// Whether this cell is SQL `NULL`.
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("NULL"),
            Self::Integer(value) => write!(f, "{value}"),
            Self::Text(value) => write!(f, "{value}"),
            Self::Boolean(value) => write!(f, "{value}"),
            Self::Ratio {
                numerator,
                denominator,
            } => write_ratio(f, *numerator, *denominator),
        }
    }
}

/// Compare two non-null scalars. Integer and ratio promote. Other mixes do not.
pub(crate) fn cmp_scalar(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => Some(left.cmp(right)),
        (Value::Text(left), Value::Text(right)) => Some(left.cmp(right)),
        (Value::Boolean(left), Value::Boolean(right)) => Some(left.cmp(right)),
        (
            Value::Ratio {
                numerator: left_n,
                denominator: left_d,
            },
            Value::Ratio {
                numerator: right_n,
                denominator: right_d,
            },
        ) => Some(cmp_ratio(*left_n, *left_d, *right_n, *right_d)),
        (
            Value::Ratio {
                numerator,
                denominator,
            },
            Value::Integer(value),
        ) => Some(cmp_ratio(*numerator, *denominator, i128::from(*value), 1)),
        (
            Value::Integer(value),
            Value::Ratio {
                numerator,
                denominator,
            },
        ) => Some(cmp_ratio(i128::from(*value), 1, *numerator, *denominator)),
        _ => None,
    }
}

/// Exact comparison of `a_num/a_den` and `b_num/b_den`.
///
/// A zero denominator is a malformed internal value and is rejected. It is
/// not compared as zero. Query results from `AVG` have a positive denominator.
/// When both denominators are positive and both cross products fit in `i128`,
/// those products decide. Otherwise the products are formed in 256 bits.
/// `(a/b) - (c/d) = (a*d - c*b) / (b*d)`, so the product order is reversed
/// when `b*d` is negative. This never uses floating point, and it does not
/// divide `i128::MIN` by `-1`.
pub(crate) fn cmp_ratio(a_num: i128, a_den: i64, b_num: i128, b_den: i64) -> std::cmp::Ordering {
    assert_nonzero_denominator(a_den);
    assert_nonzero_denominator(b_den);
    if a_den > 0
        && b_den > 0
        && let (Some(left), Some(right)) = (
            a_num.checked_mul(i128::from(b_den)),
            b_num.checked_mul(i128::from(a_den)),
        )
    {
        return left.cmp(&right);
    }
    let ordering = cross_product(a_num, b_den).cmp_signed(cross_product(b_num, a_den));
    if (a_den < 0) ^ (b_den < 0) {
        ordering.reverse()
    } else {
        ordering
    }
}

/// `numerator * denominator`, with the full product in 256 bits.
fn assert_nonzero_denominator(denominator: i64) {
    assert!(denominator != 0, "ratio denominator must not be zero");
}

fn cross_product(numerator: i128, denominator: i64) -> SignedWide {
    if numerator == 0 {
        return SignedWide::ZERO;
    }
    let (hi, lo) = widening_mul(
        numerator.unsigned_abs(),
        u128::from(denominator.unsigned_abs()),
    );
    SignedWide {
        neg: (numerator < 0) ^ (denominator < 0),
        hi,
        lo,
    }
}

#[derive(Clone, Copy)]
struct SignedWide {
    neg: bool,
    hi: u128,
    lo: u128,
}

impl SignedWide {
    const ZERO: Self = Self {
        neg: false,
        hi: 0,
        lo: 0,
    };

    fn cmp_signed(self, other: Self) -> std::cmp::Ordering {
        let magnitude = self.hi.cmp(&other.hi).then(self.lo.cmp(&other.lo));
        match (self.neg, other.neg) {
            (false, false) => magnitude,
            (true, true) => magnitude.reverse(),
            (true, false) => {
                if self.hi == 0 && self.lo == 0 && other.hi == 0 && other.lo == 0 {
                    std::cmp::Ordering::Equal
                } else {
                    std::cmp::Ordering::Less
                }
            }
            (false, true) => {
                if self.hi == 0 && self.lo == 0 && other.hi == 0 && other.lo == 0 {
                    std::cmp::Ordering::Equal
                } else {
                    std::cmp::Ordering::Greater
                }
            }
        }
    }
}

/// `a * b` as a 256-bit value. `hi` holds the upper 128 bits.
fn widening_mul(a: u128, b: u128) -> (u128, u128) {
    const MASK: u128 = (1 << 64) - 1;
    let a0 = a & MASK;
    let a1 = a >> 64;
    let b0 = b & MASK;
    let b1 = b >> 64;
    let p00 = a0 * b0;
    let p01 = a0 * b1;
    let p10 = a1 * b0;
    let p11 = a1 * b1;
    let mut mid = (p00 >> 64) + (p01 & MASK) + (p10 & MASK);
    let hi = p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64);
    mid &= MASK;
    (hi, (mid << 64) | (p00 & MASK))
}

fn normalize_ratio(numerator: i128, denominator: i64) -> (i128, i64) {
    if numerator == 0 {
        return (0, 1);
    }
    let negative = (numerator < 0) ^ (denominator < 0);
    let mut num = numerator.unsigned_abs();
    let mut den = u128::from(denominator.unsigned_abs());
    let divisor = gcd(num, den);
    num /= divisor;
    den /= divisor;
    canonical(negative, num, den).unwrap_or((numerator, denominator))
}

/// Reduced form with a positive denominator, when that form fits.
///
/// `+2^127` does not fit in `i128`, and `2^63` does not fit in a positive
/// `i64`. Those two inputs stay in their original bits. `AVG` never builds
/// them: its count is in `1..=i64::MAX` and its sum already fits in `i128`.
fn canonical(negative: bool, num: u128, den: u128) -> Option<(i128, i64)> {
    let denominator = i64::try_from(den).ok()?;
    if denominator <= 0 {
        return None;
    }
    let numerator = if !negative {
        i128::try_from(num).ok()?
    } else if num == 1_u128 << 127 {
        i128::MIN
    } else {
        -i128::try_from(num).ok()?
    };
    Some((numerator, denominator))
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

fn write_ratio(f: &mut fmt::Formatter<'_>, numerator: i128, denominator: i64) -> fmt::Result {
    assert_nonzero_denominator(denominator);
    if numerator == 0 {
        return f.write_str("0.0");
    }
    let negative = (numerator < 0) ^ (denominator < 0);
    let mut num = numerator.unsigned_abs();
    let mut den = u128::from(denominator.unsigned_abs());
    let divisor = gcd(num, den);
    num /= divisor;
    den /= divisor;
    if decimal_terminates(den) {
        write_decimal(f, negative, num, den)
    } else {
        write_fraction(f, negative, num, den)
    }
}

/// `|denominator|` is at most `2^63`, so a terminating expansion has at most
/// 63 digits. The loop below stops at 64 and prints the reduced fraction if a
/// remainder is still left, which keeps a malformed value from looping or
/// allocating without a bound.
fn decimal_terminates(mut denominator: u128) -> bool {
    if denominator == 0 {
        return false;
    }
    while denominator.is_multiple_of(2) {
        denominator /= 2;
    }
    while denominator.is_multiple_of(5) {
        denominator /= 5;
    }
    denominator == 1
}

fn write_decimal(
    f: &mut fmt::Formatter<'_>,
    negative: bool,
    numerator: u128,
    denominator: u128,
) -> fmt::Result {
    if denominator == 1 {
        return write_whole(f, negative, numerator);
    }
    let whole = numerator / denominator;
    let mut rem = numerator % denominator;
    if rem == 0 {
        return write_whole(f, negative, whole);
    }
    let mut digits = String::new();
    for _ in 0..64 {
        if rem == 0 {
            break;
        }
        let Some(next) = rem.checked_mul(10) else {
            return write_fraction(f, negative, numerator, denominator);
        };
        rem = next;
        let digit = rem / denominator;
        rem %= denominator;
        let Some(digit) = u32::try_from(digit)
            .ok()
            .and_then(|digit| char::from_digit(digit, 10))
        else {
            return write_fraction(f, negative, numerator, denominator);
        };
        digits.push(digit);
    }
    if rem != 0 {
        return write_fraction(f, negative, numerator, denominator);
    }
    if negative {
        write!(f, "-{whole}.{digits}")
    } else {
        write!(f, "{whole}.{digits}")
    }
}

fn write_whole(f: &mut fmt::Formatter<'_>, negative: bool, whole: u128) -> fmt::Result {
    if whole == 0 {
        return f.write_str("0.0");
    }
    if negative {
        write!(f, "-{whole}.0")
    } else {
        write!(f, "{whole}.0")
    }
}

fn write_fraction(
    f: &mut fmt::Formatter<'_>,
    negative: bool,
    numerator: u128,
    denominator: u128,
) -> fmt::Result {
    if negative {
        write!(f, "-{numerator}/{denominator}")
    } else {
        write!(f, "{numerator}/{denominator}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_scalars_without_quotes() {
        assert_eq!(Value::Null.to_string(), "NULL");
        assert_eq!(Value::Integer(-12).to_string(), "-12");
        assert_eq!(Value::Text("Bob".to_string()).to_string(), "Bob");
        assert_eq!(Value::Boolean(true).to_string(), "true");
        assert_eq!(Value::Boolean(false).to_string(), "false");
    }

    #[test]
    fn reports_runtime_types() {
        assert_eq!(Value::Null.data_type(), None);
        assert!(Value::Null.is_null());
        assert_eq!(Value::Integer(1).data_type(), Some(DataType::Integer));
        assert_eq!(Value::Text(String::new()).data_type(), Some(DataType::Text));
        assert_eq!(Value::Boolean(false).data_type(), Some(DataType::Boolean));
        assert_eq!(
            Value::ratio(1, 2).data_type(),
            None,
            "a ratio is not a column type"
        );
        assert_eq!(DataType::Boolean.to_string(), "BOOLEAN");
    }

    #[test]
    fn reduces_and_formats_ratios_without_floats() {
        assert_eq!(Value::ratio(2, 4).to_string(), "0.5");
        assert_eq!(Value::ratio(4, 1).to_string(), "4.0");
        assert_eq!(Value::ratio(-4, 1).to_string(), "-4.0");
        assert_eq!(Value::ratio(1, 3).to_string(), "1/3");
        assert_eq!(Value::ratio(-1, 3).to_string(), "-1/3");
        assert_eq!(Value::ratio(-5, 2).to_string(), "-2.5");
        assert_eq!(Value::ratio(0, 5).to_string(), "0.0");
        assert_eq!(
            Value::ratio(i128::from(i64::MIN), 1).to_string(),
            format!("{}.0", i64::MIN)
        );
        assert_eq!(
            Value::ratio(i128::from(i64::MAX), 1).to_string(),
            format!("{}.0", i64::MAX)
        );
        let value = Value::ratio(2, 4);
        let Value::Ratio {
            numerator,
            denominator,
        } = value
        else {
            panic!("expected a ratio");
        };
        assert_eq!((numerator, denominator), (1, 2));
    }

    #[test]
    fn compares_ratios_that_overflow_a_cross_product() {
        let huge = Value::ratio(i128::MAX, 3);
        let half = Value::ratio(1, 2);
        assert_eq!(cmp_scalar(&huge, &half), Some(std::cmp::Ordering::Greater));
        assert_eq!(
            cmp_scalar(&Value::ratio(-i128::MAX, 3), &Value::ratio(-1, 2)),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(
            cmp_scalar(&Value::ratio(1, 2), &Value::Integer(1)),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(
            cmp_ratio(i128::MAX, 3, i128::MAX, 3),
            std::cmp::Ordering::Equal
        );
        assert!(i128::MAX.checked_mul(2).is_none());
    }

    #[test]
    fn ratio_is_not_a_persistent_value() {
        let error = Value::ratio(1, 2).ensure_persistent().unwrap_err();
        assert_eq!(
            error.to_string(),
            "internal error: ratio values are not stored"
        );
        assert!(Value::Integer(1).ensure_persistent().is_ok());
    }

    /// Schoolbook integers. This is the oracle for comparison and display.
    /// It does not share the production multiply or the production formatter.
    #[derive(Clone)]
    struct Big {
        neg: bool,
        limbs: Vec<u32>,
    }

    impl Big {
        fn from_i128(value: i128) -> Self {
            let neg = value < 0;
            let mut mag = value.unsigned_abs();
            let mut limbs = Vec::new();
            while mag > 0 {
                limbs.push((mag & u128::from(u32::MAX)) as u32);
                mag >>= 32;
            }
            Self { neg, limbs }
        }

        fn from_digits(text: &str) -> Self {
            let mut value = Self::zero();
            let ten = Self::from_u64(10);
            for byte in text.bytes() {
                assert!(byte.is_ascii_digit(), "{text}");
                value = value.mul(&ten).add_u32(u32::from(byte - b'0'));
            }
            value
        }

        fn pow10(exp: usize) -> Self {
            let mut value = Self::from_u64(1);
            let ten = Self::from_u64(10);
            for _ in 0..exp {
                value = value.mul(&ten);
            }
            value
        }

        fn add_u32(&self, extra: u32) -> Self {
            let mut limbs = self.limbs.clone();
            if limbs.is_empty() {
                limbs.push(0);
            }
            let mut carry = u64::from(extra);
            for limb in &mut limbs {
                let sum = u64::from(*limb) + carry;
                *limb = sum as u32;
                carry = sum >> 32;
                if carry == 0 {
                    break;
                }
            }
            if carry > 0 {
                limbs.push(carry as u32);
            }
            Self {
                neg: self.neg,
                limbs,
            }
        }

        fn from_u64(value: u64) -> Self {
            let mut limbs = Vec::new();
            if value > 0 {
                limbs.push(value as u32);
                let hi = (value >> 32) as u32;
                if hi > 0 {
                    limbs.push(hi);
                }
            }
            Self { neg: false, limbs }
        }

        fn zero() -> Self {
            Self {
                neg: false,
                limbs: Vec::new(),
            }
        }

        fn is_zero(&self) -> bool {
            self.limbs.iter().all(|limb| *limb == 0)
        }

        fn mul(&self, other: &Self) -> Self {
            if self.is_zero() || other.is_zero() {
                return Self::zero();
            }
            let mut limbs = vec![0u64; self.limbs.len() + other.limbs.len()];
            for (i, left) in self.limbs.iter().copied().enumerate() {
                let mut carry = 0u64;
                for (j, right) in other.limbs.iter().copied().enumerate() {
                    let sum = limbs[i + j] + u64::from(left) * u64::from(right) + carry;
                    limbs[i + j] = sum & u64::from(u32::MAX);
                    carry = sum >> 32;
                }
                limbs[i + other.limbs.len()] += carry;
            }
            while limbs.last().is_some_and(|limb| *limb == 0) {
                limbs.pop();
            }
            Self {
                neg: self.neg ^ other.neg,
                limbs: limbs.into_iter().map(|limb| limb as u32).collect(),
            }
        }

        fn cmp_mag(&self, other: &Self) -> std::cmp::Ordering {
            let left = significant(&self.limbs);
            let right = significant(&other.limbs);
            left.len().cmp(&right.len()).then_with(|| left.cmp(&right))
        }

        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            match (self.is_zero(), other.is_zero()) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => {
                    if other.neg {
                        std::cmp::Ordering::Greater
                    } else {
                        std::cmp::Ordering::Less
                    }
                }
                (false, true) => {
                    if self.neg {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    }
                }
                (false, false) if self.neg != other.neg => {
                    if self.neg {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    }
                }
                (false, false) => {
                    let mag = self.cmp_mag(other);
                    if self.neg { mag.reverse() } else { mag }
                }
            }
        }
    }

    fn significant(limbs: &[u32]) -> Vec<u32> {
        limbs
            .iter()
            .rev()
            .copied()
            .skip_while(|limb| *limb == 0)
            .collect()
    }

    fn oracle_cmp(a_num: i128, a_den: i64, b_num: i128, b_den: i64) -> std::cmp::Ordering {
        assert!(a_den != 0 && b_den != 0, "the oracle does not define n/0");
        // (a/b) - (c/d) = (a*d - c*b) / (b*d). The cross-product order matches
        // the values when b*d is positive, and flips when b*d is negative.
        let left = signed_product(a_num, b_den);
        let right = signed_product(b_num, a_den);
        let ordering = left.cmp(&right);
        if (a_den < 0) ^ (b_den < 0) {
            ordering.reverse()
        } else {
            ordering
        }
    }

    fn signed_product(numerator: i128, denominator: i64) -> Big {
        let mut value = Big::from_i128(numerator).mul(&Big::from_u64(denominator.unsigned_abs()));
        if denominator < 0 && !value.is_zero() {
            value.neg = !value.neg;
        }
        value
    }

    fn assert_cmp(a_num: i128, a_den: i64, b_num: i128, b_den: i64) {
        let actual = cmp_ratio(a_num, a_den, b_num, b_den);
        let expected = oracle_cmp(a_num, a_den, b_num, b_den);
        assert_eq!(
            actual, expected,
            "{a_num}/{a_den} ? {b_num}/{b_den}: engine {actual:?}, oracle {expected:?}"
        );
        assert_eq!(cmp_ratio(b_num, b_den, a_num, a_den), expected.reverse());
        assert_eq!(
            cmp_ratio(a_num, a_den, a_num, a_den),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn ratio_comparison_matches_an_independent_bigint() {
        let samples = [
            (0, 1),
            (0, -3),
            (1, 1),
            (-1, 1),
            (1, 2),
            (-1, 2),
            (1, -2),
            (-1, -2),
            (2, 4),
            (-2, 4),
            (2, -4),
            (1, 3),
            (-5, 2),
            (5, -2),
            (i128::from(i64::MIN), 1),
            (i128::from(i64::MAX), 1),
            (i128::from(i64::MIN), 2),
            (i128::from(i64::MAX), 2),
            (i128::MIN, 1),
            (i128::MAX, 1),
            (i128::MIN, 2),
            (i128::MAX, 2),
            (i128::MIN, 3),
            (i128::MAX, 3),
            (-i128::MAX, 3),
            (i128::MAX, i64::MAX),
            (-i128::MAX, i64::MAX),
            (i128::MAX - 1, i64::MAX),
            (1, i64::MAX),
            (-1, i64::MAX),
            (i128::MIN, -1),
            (i128::MIN, -2),
            (i128::MIN, -3),
            (1, i64::MIN),
            (2, i64::MIN),
            (-3, i64::MIN),
            (i128::MAX, i64::MIN),
            (0, i64::MIN),
        ];
        for (a_num, a_den) in samples {
            for (b_num, b_den) in samples {
                assert_cmp(a_num, a_den, b_num, b_den);
            }
        }

        let mut state = 0x12_u64;
        let mut next = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state
        };
        let mut seen = Vec::new();
        for _ in 0..80 {
            let num = i128::from(next() as i64) ^ (i128::from(next() as i32) << 64);
            let den = match next() % 7 {
                0 => i64::MIN,
                1 => -1,
                2 => 1,
                3 => i64::MAX,
                _ => (next() as i64) | 1,
            };
            seen.push((num, den));
            for (other_num, other_den) in &seen {
                assert_cmp(num, den, *other_num, *other_den);
            }
        }
        // Near-equal fractions around a power-of-two boundary.
        let base = i128::from(i64::MAX);
        for delta in -3..=3 {
            assert_cmp(base + i128::from(delta), 1, base, 1);
            assert_cmp(base + i128::from(delta), 3, base, 3);
            assert_cmp(-(base + i128::from(delta)), 3, -base, 2);
        }
        // Differently written forms of the same value.
        assert_eq!(cmp_ratio(2, 4, 1, 2), std::cmp::Ordering::Equal);
        assert_eq!(cmp_ratio(1, -2, -1, 2), std::cmp::Ordering::Equal);
        assert_eq!(
            cmp_ratio(i128::MIN, -2, 1_i128 << 126, 1),
            std::cmp::Ordering::Equal
        );
        assert_eq!(cmp_ratio(0, 5, 0, -1), std::cmp::Ordering::Equal);
        assert_eq!(
            cmp_scalar(&Value::ratio(4, 2), &Value::Integer(2)),
            Some(std::cmp::Ordering::Equal)
        );
        assert_eq!(
            cmp_scalar(&Value::ratio(-5, 2), &Value::Integer(-2)),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(
            cmp_scalar(&Value::ratio(-5, 2), &Value::Integer(-3)),
            Some(std::cmp::Ordering::Greater)
        );
        assert_eq!(cmp_scalar(&Value::Null, &Value::ratio(1, 2)), None);
        let mut chain: Vec<(i128, i64)> = seen.into_iter().take(12).collect();
        chain.sort_by(|left, right| cmp_ratio(left.0, left.1, right.0, right.1));
        for window in chain.windows(3) {
            let ab = cmp_ratio(window[0].0, window[0].1, window[1].0, window[1].1);
            let bc = cmp_ratio(window[1].0, window[1].1, window[2].0, window[2].1);
            let ac = cmp_ratio(window[0].0, window[0].1, window[2].0, window[2].1);
            if ab != std::cmp::Ordering::Greater && bc != std::cmp::Ordering::Greater {
                assert_ne!(ac, std::cmp::Ordering::Greater);
            }
        }
    }

    #[test]
    fn a_zero_denominator_is_rejected_and_not_compared_as_zero() {
        let message = panic_message(|| {
            Value::ratio(1, 0);
        });
        assert!(
            message.contains("ratio denominator must not be zero"),
            "{message}"
        );
        let message = panic_message(|| {
            cmp_ratio(1, 0, 0, 1);
        });
        assert!(
            message.contains("ratio denominator must not be zero"),
            "{message}"
        );
        let message = panic_message(|| {
            cmp_scalar(
                &Value::Ratio {
                    numerator: 5,
                    denominator: 0,
                },
                &Value::Integer(0),
            );
        });
        assert!(
            message.contains("ratio denominator must not be zero"),
            "{message}"
        );
        let message = panic_message(|| {
            Value::Ratio {
                numerator: 5,
                denominator: 0,
            }
            .to_string();
        });
        assert!(
            message.contains("ratio denominator must not be zero"),
            "{message}"
        );
        let negative = Value::ratio(1, i64::MIN);
        assert_eq!(
            cmp_scalar(&negative, &Value::Integer(0)),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(
            cmp_scalar(&Value::ratio(i128::MIN, -1), &Value::Integer(0)),
            Some(std::cmp::Ordering::Greater)
        );
    }

    fn panic_message(body: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let error = std::panic::catch_unwind(body).expect_err("zero denominator was accepted");
        error
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| error.downcast_ref::<&str>().map(|text| (*text).to_string()))
            .unwrap_or_else(|| "panic had no message".to_string())
    }

    #[test]
    fn formatted_ratios_match_the_fraction_and_stay_bounded() {
        let samples = [
            (0, 1, "0.0"),
            (0, -4, "0.0"),
            (4, 1, "4.0"),
            (-4, 1, "-4.0"),
            (1, 2, "0.5"),
            (-1, 2, "-0.5"),
            (1, -2, "-0.5"),
            (-5, 2, "-2.5"),
            (5, -2, "-2.5"),
            (2, 4, "0.5"),
            (2, 6, "1/3"),
            (-2, 6, "-1/3"),
            (1, 3, "1/3"),
            (-1, 3, "-1/3"),
            (1, 8, "0.125"),
            (1, 16, "0.0625"),
            (1, 1_000, "0.001"),
            (i128::from(i64::MIN), 1, ""),
            (i128::from(i64::MAX), 1, ""),
            (i128::MIN, -2, ""),
            (1, i64::MIN, ""),
            (i128::MAX, 3, ""),
            (-i128::MAX, 3, ""),
        ];
        for (num, den, exact) in samples {
            let text = Value::ratio(num, den).to_string();
            assert!(text.len() <= 80, "{num}/{den} formatted as {text}");
            assert_ne!(text, "-0.0");
            assert!(!text.contains("e") && !text.contains("E"), "{text}");
            if !exact.is_empty() {
                assert_eq!(text, exact, "{num}/{den}");
            }
            assert_rendered_equals(num, den, &text);
        }
        let tiny = Value::ratio(1, 1_i64 << 62).to_string();
        assert_eq!(tiny.len(), 64, "{tiny}");
        assert!(tiny.starts_with("0."));
        assert_rendered_equals(1, 1_i64 << 62, &tiny);
        let min_den = Value::ratio(1, i64::MIN).to_string();
        assert!(min_den.starts_with("-0."));
        assert_eq!(min_den.len(), 66, "{min_den}");
        let Value::Ratio {
            numerator,
            denominator,
        } = Value::ratio(i128::MIN, -2)
        else {
            panic!("expected a ratio");
        };
        assert_eq!((numerator, denominator), (1_i128 << 126, 1));
        let Value::Ratio {
            numerator,
            denominator,
        } = Value::ratio(1, i64::MIN)
        else {
            panic!("expected a ratio");
        };
        assert_eq!((numerator, denominator), (1, i64::MIN));
    }

    fn assert_rendered_equals(numerator: i128, denominator: i64, text: &str) {
        let rendered = parse_rendered(text);
        let original = rendered_from_ratio(numerator, denominator);
        let left = rendered.num.mul(&original.den);
        let right = original.num.mul(&rendered.den);
        assert_eq!(
            left.cmp(&right),
            std::cmp::Ordering::Equal,
            "{text} is not {numerator}/{denominator}"
        );
    }

    struct Rendered {
        num: Big,
        den: Big,
    }

    fn rendered_from_ratio(numerator: i128, denominator: i64) -> Rendered {
        assert!(denominator != 0, "the oracle does not define n/0");
        Rendered {
            num: Big::from_i128(numerator),
            den: Big::from_i128(i128::from(denominator)),
        }
    }

    fn parse_rendered(text: &str) -> Rendered {
        let negative = text.starts_with('-');
        let body = text.strip_prefix('-').unwrap_or(text);
        let (num, den) = if let Some((num, den)) = body.split_once('/') {
            (Big::from_digits(num), Big::from_digits(den))
        } else {
            let (whole, frac) = body.split_once('.').expect("decimal point");
            assert!(!frac.is_empty() && frac.len() <= 64, "{text}");
            if frac != "0" {
                assert!(!frac.ends_with('0'), "trailing zero in {text}");
            }
            (
                Big::from_digits(&format!("{whole}{frac}")),
                Big::pow10(frac.len()),
            )
        };
        Rendered {
            num: signed_big(num, negative),
            den,
        }
    }

    fn signed_big(mut value: Big, negative: bool) -> Big {
        if negative && !value.is_zero() {
            value.neg = true;
        }
        value
    }
}
