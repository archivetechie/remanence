//! Exact rational arithmetic for longitudinal tape positions.
//!
//! The mapping in design-read-ordering.md §6.4 produces a longitudinal
//! fraction per block whose denominator is the span of the block's wrap.
//! Those fractions are stored and compared exactly — no floating point
//! anywhere in stored or compared values. Comparison uses a
//! continued-fraction descent rather than cross-multiplication, so it is
//! exact for the full `i128` range without overflow.

use std::cmp::Ordering;

/// Greatest common divisor over `u128`, by the binary-free Euclid loop.
pub(crate) const fn gcd_u128(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// An exact rational number with a strictly positive denominator, kept
/// reduced to lowest terms so that `PartialEq`/`Hash` agree with `Ord`.
///
/// Longitudinal positions are usually in `[0, 1]`, but the estimated
/// EOD-wrap denominator of §6.4 can put a block's fraction above one, and
/// `1 - frac` on a reverse wrap can then be negative. The type is signed
/// for exactly that case; the cost model only ever consumes absolute
/// differences.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ratio {
    num: i128,
    den: i128,
}

impl Ratio {
    /// Exact zero.
    pub const ZERO: Ratio = Ratio { num: 0, den: 1 };
    /// Exact one.
    pub const ONE: Ratio = Ratio { num: 1, den: 1 };

    /// Build `num / den` reduced to lowest terms.
    ///
    /// Returns `None` when `den == 0`. A negative `den` is normalised so
    /// the stored denominator is always positive.
    pub fn new(num: i128, den: i128) -> Option<Ratio> {
        if den == 0 {
            return None;
        }
        Some(Self::reduced(num, den))
    }

    fn reduced(mut num: i128, mut den: i128) -> Ratio {
        debug_assert!(den != 0);
        if den < 0 {
            // `den == i128::MIN` cannot be negated; it never occurs because
            // every constructor site passes a `u64`-derived denominator.
            num = -num;
            den = -den;
        }
        let g = gcd_u128(num.unsigned_abs(), den.unsigned_abs());
        if g > 1 {
            num /= g as i128;
            den /= g as i128;
        }
        Ratio { num, den }
    }

    /// Numerator of the reduced form; carries the sign.
    pub fn num(self) -> i128 {
        self.num
    }

    /// Denominator of the reduced form; always strictly positive.
    pub fn den(self) -> i128 {
        self.den
    }

    /// True when the value is strictly negative.
    pub fn is_negative(self) -> bool {
        self.num < 0
    }

    /// `1 - self`, exactly.
    ///
    /// `None` only on `i128` overflow, which is unreachable for values
    /// built from `u64` block offsets and spans.
    pub fn checked_one_minus(self) -> Option<Ratio> {
        let num = self.den.checked_sub(self.num)?;
        Some(Self::reduced(num, self.den))
    }

    /// `|self - other|`, exactly.
    ///
    /// `None` only on `i128` overflow of the cross products, which is
    /// unreachable for physically plausible wrap spans; the cost model
    /// maps it to a saturated cost rather than a panic.
    pub fn checked_abs_diff(self, other: Ratio) -> Option<Ratio> {
        let a = self.num.checked_mul(other.den)?;
        let b = other.num.checked_mul(self.den)?;
        let num = a.checked_sub(b)?.checked_abs()?;
        let den = self.den.checked_mul(other.den)?;
        Some(Self::reduced(num, den))
    }
}

impl Ord for Ratio {
    /// Exact comparison by continued-fraction descent.
    ///
    /// Compares integer parts; on a tie, the fractional parts `r/d` are
    /// compared through their reciprocals, which strictly shrinks the
    /// denominators (Euclid) and therefore terminates. No intermediate
    /// can overflow.
    fn cmp(&self, other: &Self) -> Ordering {
        let (mut an, mut ad) = (self.num, self.den);
        let (mut bn, mut bd) = (other.num, other.den);
        loop {
            // Denominators are positive, so Euclidean quotient/remainder
            // give the floor and a remainder in [0, den).
            let (qa, ra) = (an.div_euclid(ad), an.rem_euclid(ad));
            let (qb, rb) = (bn.div_euclid(bd), bn.rem_euclid(bd));
            match qa.cmp(&qb) {
                Ordering::Equal => {}
                ord => return ord,
            }
            match (ra == 0, rb == 0) {
                (true, true) => return Ordering::Equal,
                (true, false) => return Ordering::Less,
                (false, true) => return Ordering::Greater,
                // ra/ad vs rb/bd, both in (0, 1):
                // ra/ad < rb/bd  <=>  bd/rb < ad/ra.
                (false, false) => {
                    let (nan, nad, nbn, nbd) = (bd, rb, ad, ra);
                    an = nan;
                    ad = nad;
                    bn = nbn;
                    bd = nbd;
                }
            }
        }
    }
}

impl PartialOrd for Ratio {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// `floor(scale * num / den)` with `u128` intermediates.
///
/// Reduces both factors against the denominator first; if the product
/// still overflows, splits `num` by the denominator. Returns `None` only
/// when even the split overflows, which requires denominators beyond any
/// physically plausible wrap span; callers saturate.
pub(crate) fn mul_div_floor_u128(scale: u128, num: u128, den: u128) -> Option<u128> {
    debug_assert!(den != 0);
    let g1 = gcd_u128(scale, den);
    let (scale, den) = (scale / g1, den / g1);
    let g2 = gcd_u128(num, den);
    let (num, den) = (num / g2, den / g2);
    if let Some(p) = scale.checked_mul(num) {
        return Some(p / den);
    }
    // Split num = q*den + r with r < den.
    let (q, r) = (num / den, num % den);
    let whole = scale.checked_mul(q)?;
    let part = scale.checked_mul(r)? / den;
    whole.checked_add(part)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduction_is_canonical() {
        assert_eq!(Ratio::new(2, 4), Ratio::new(1, 2));
        assert_eq!(Ratio::new(-2, 4), Ratio::new(1, -2));
        assert_eq!(Ratio::new(0, 7).unwrap(), Ratio::ZERO);
        assert!(Ratio::new(1, 0).is_none());
    }

    #[test]
    fn exact_compare_without_overflow() {
        // Values whose cross products overflow i128 still compare exactly.
        let big = (1i128 << 63) - 1;
        let a = Ratio::new(big - 1, big).unwrap();
        let b = Ratio::new(big - 2, big - 1).unwrap();
        // (big-1)/big > (big-2)/(big-1) because 1/big < 1/(big-1) below one.
        assert!(a > b);
        let c = Ratio::new(1, big).unwrap();
        let d = Ratio::new(1, big - 1).unwrap();
        assert!(c < d);
        assert_eq!(a.cmp(&a), Ordering::Equal);
    }

    #[test]
    fn one_minus_and_abs_diff() {
        let f = Ratio::new(3, 10).unwrap();
        assert_eq!(f.checked_one_minus().unwrap(), Ratio::new(7, 10).unwrap());
        // Overshooting fraction goes negative under 1 - frac.
        let over = Ratio::new(13, 10).unwrap();
        let lpos = over.checked_one_minus().unwrap();
        assert!(lpos.is_negative());
        assert_eq!(lpos, Ratio::new(-3, 10).unwrap());
        let d = Ratio::new(1, 3)
            .unwrap()
            .checked_abs_diff(Ratio::new(1, 4).unwrap())
            .unwrap();
        assert_eq!(d, Ratio::new(1, 12).unwrap());
    }

    #[test]
    fn mul_div_floor_matches_exact_small_cases() {
        assert_eq!(mul_div_floor_u128(99, 1, 3), Some(33));
        assert_eq!(mul_div_floor_u128(99, 2, 3), Some(66));
        assert_eq!(mul_div_floor_u128(100, 1, 3), Some(33));
        assert_eq!(mul_div_floor_u128(0, 5, 7), Some(0));
    }
}
