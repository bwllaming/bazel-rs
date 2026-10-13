//! Independently authored exact binary64 -> closest-even decimal selection.
//!
//! Behavioral specification: OpenJDK 25 Double.toString (minimum canonical
//! length, except the length-one/two union, then closest/even), composed with
//! Bazel 9.3 StarlarkFloat's g(17) layout. No JDK implementation is copied.
//! Each binary value is U/2^1074; midpoint endpoints use its actual neighbors,
//! including the conceptual 2^1024 overflow neighbor. On each decimal grid the
//! two brackets dominate all more distant candidates. Searching grids 1..=17
//! therefore finds the minimum length; exact cross-products select closest/even.
//! A 17-digit grid has half-spacing <=5e-17*x < 2^-54*x, so it intersects the
//! rounding interval even at asymmetric binade boundaries. All arithmetic is
//! integer arithmetic; parsing/host float formatting never select a decimal.

use super::{Error, JsonBudget, Result};
use num_bigint::BigUint;
use std::cmp::Ordering;

const MAX: u64 = 0x7fefffffffffffff;
const MAG: u64 = 0x7fffffffffffffff;

fn units(bits: u64) -> Result<BigUint> {
    if bits > MAX {
        return Err(Error::Internal("float units domain"));
    }
    let e = (bits >> 52) as usize;
    let m = bits & ((1u64 << 52) - 1);
    Ok(if e == 0 {
        BigUint::from(m)
    } else {
        BigUint::from(m | (1u64 << 52)) << (e - 1)
    })
}

pub(super) struct Math {
    ten: Vec<BigUint>,
    den: BigUint,
    midpoint_den: BigUint,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Decimal {
    coefficient: u64,
    power: i32,
}

impl Decimal {
    fn new(mut coefficient: u64, mut power: i32) -> Result<Self> {
        if coefficient == 0 {
            return Err(Error::Internal("zero decimal coefficient"));
        }
        while coefficient.is_multiple_of(10) {
            coefficient /= 10;
            power += 1;
        }
        Ok(Self { coefficient, power })
    }
    fn length(self) -> usize {
        self.coefficient.ilog10() as usize + 1
    }
}

struct Interval {
    u: BigUint,
    low: BigUint,
    high: BigUint,
    closed: bool,
}

impl Math {
    pub(super) fn new(budget: &mut JsonBudget) -> Result<Self> {
        // Working reservation, not cumulative allocator traffic or an RSS/CPU
        // promise: 341 table entries * (32 header + 2*144 limb-capacity bytes),
        // rounded to 128 KiB; another 128 KiB covers 32 live 2-KiB operands/
        // results plus 64 KiB of multiplication/division temporary workspace.
        // Mathematical intermediates are <4358 bits (8192 conservative ceiling),
        // operands of multiplication <3229 bits. This leaves room for doubled
        // vector capacities and num-bigint 0.4.8 scratch. Specifically,
        // biguint/multiplication.rs::mac3 uses schoolbook for <=32 digits,
        // half-Karatsuba for disproportionate lengths, otherwise Karatsuba up
        // to 256 digits. Here the smaller factor is always <=1130 bits (36
        // 32-bit digits, or 18 64-bit digits), the larger <=3229 bits: no Toom-3,
        // at most a half split followed by one Karatsuba level. Its product
        // buffer and two differences are well below the 64-KiB scratch reserve.
        // division.rs::div_rem_core switches to recursive division only when
        // dividend >128 digits AND divisor >64 digits. Our <=3228-bit dividend
        // (<=101 32-bit digits, plus one normalization digit) never does; the
        // normalized operand copies, quotient, remainder use <8 operand slots.
        // The working area is reserved once per operation (temporaries do not
        // escape conversion). Each conversion also pays a cumulative work fee.
        budget.bytes(256 * 1024)?;
        let mut ten = Vec::new();
        ten.try_reserve_exact(341).map_err(|_| Error::Resource)?;
        ten.push(BigUint::from(1u8));
        for i in 1..=340 {
            ten.push(&ten[i - 1] * 10u8);
        }
        Ok(Self {
            ten,
            den: BigUint::from(1u8) << 1074usize,
            midpoint_den: BigUint::from(1u8) << 1075usize,
        })
    }
    fn power(&self, exponent: i32) -> Result<&BigUint> {
        let index = exponent.unsigned_abs() as usize;
        self.ten
            .get(index)
            .ok_or(Error::Internal("decimal exponent domain"))
    }
    fn interval(&self, bits: u64) -> Result<Interval> {
        if !(1..=MAX).contains(&bits) {
            return Err(Error::Internal("float interval domain"));
        }
        let u = units(bits)?;
        let previous = units(bits - 1)?;
        let next = if bits == MAX {
            BigUint::from(1u8) << 2098usize
        } else {
            units(bits + 1)?
        };
        Ok(Interval {
            low: &u + previous,
            high: &u + next,
            u,
            closed: bits & 1 == 0,
        })
    }
    fn ratio(&self, c: Decimal) -> Result<(BigUint, BigUint)> {
        if !(-340..=309).contains(&c.power) {
            return Err(Error::Internal("normalized decimal domain"));
        }
        Ok(if c.power >= 0 {
            (
                BigUint::from(c.coefficient) * self.power(c.power)?,
                BigUint::from(1u8),
            )
        } else {
            (BigUint::from(c.coefficient), self.power(c.power)?.clone())
        })
    }
    fn contains(&self, i: &Interval, c: Decimal) -> Result<bool> {
        let (a, b) = self.ratio(c)?;
        let value = a * &self.midpoint_den;
        let low = &i.low * &b;
        let high = &i.high * b;
        Ok(if i.closed {
            low <= value && value <= high
        } else {
            low < value && value < high
        })
    }
    fn decimal_exponent(&self, u: &BigUint) -> Result<i32> {
        let (mut low, mut high) = (-324, 309);
        while high - low > 1 {
            let mid = low + (high - low) / 2;
            let ordering = if mid >= 0 {
                u.cmp(&(&self.den * self.power(mid)?))
            } else {
                (u * self.power(mid)?).cmp(&self.den)
            };
            if ordering == Ordering::Less {
                high = mid;
            } else {
                low = mid;
            }
        }
        Ok(low)
    }
    fn candidates(&self, i: &Interval, k: i32, p: usize) -> Result<[Option<Decimal>; 2]> {
        let q = k - p as i32 + 1;
        let (a, b) = if q >= 0 {
            (i.u.clone(), &self.den * self.power(q)?)
        } else {
            (&i.u * self.power(q)?, self.den.clone())
        };
        let floor = &a / &b;
        let mut words = floor.iter_u64_digits();
        let floor = words
            .next()
            .ok_or(Error::Internal("empty decimal quotient"))?;
        if words.next().is_some() || floor == 0 || floor >= 100_000_000_000_000_000 {
            return Err(Error::Internal("decimal quotient domain"));
        }
        let mut out = [None, None];
        for (slot, coefficient) in out.iter_mut().zip([floor, floor + 1]) {
            let c = Decimal::new(coefficient, q)?;
            if c.length() <= p && self.contains(i, c)? {
                *slot = Some(c);
            }
        }
        Ok(out)
    }
    fn distance(&self, i: &Interval, c: Decimal) -> Result<(BigUint, BigUint)> {
        let (a, b) = self.ratio(c)?;
        let x = a * &self.den;
        let y = &i.u * &b;
        Ok((if x >= y { x - y } else { y - x }, b))
    }
    fn select(&self, bits: u64) -> Result<Decimal> {
        let i = self.interval(bits)?;
        let k = self.decimal_exponent(&i.u)?;
        for p in 1..=17 {
            let first = self.candidates(&i, k, p)?;
            if first.iter().all(Option::is_none) {
                continue;
            }
            let second = if p == 1 {
                self.candidates(&i, k, 2)?
            } else {
                [None, None]
            };
            let mut best = None;
            for c in first.into_iter().chain(second).flatten() {
                let Some(old) = best else {
                    best = Some(c);
                    continue;
                };
                if c == old {
                    continue;
                }
                let (a, b) = self.distance(&i, c)?;
                let (x, y) = self.distance(&i, old)?;
                match (a * y).cmp(&(x * b)) {
                    Ordering::Less => best = Some(c),
                    Ordering::Equal => {
                        if c.coefficient % 2 == old.coefficient % 2 {
                            return Err(Error::Internal("ambiguous decimal tie"));
                        }
                        if c.coefficient % 2 == 0 {
                            best = Some(c);
                        }
                    }
                    Ordering::Greater => {}
                }
            }
            let best = best.ok_or(Error::Internal("missing decimal candidate"))?;
            if !self.contains(&i, best)? {
                return Err(Error::Internal("selected decimal outside interval"));
            }
            return Ok(best);
        }
        Err(Error::Internal("17-digit decimal existence"))
    }
    pub(super) fn format(&self, value: f64, budget: &mut JsonBudget) -> Result<String> {
        // <=200 multiplications, <=17 divisions, <=34 brackets, <=4 retained
        // candidates; this fixed fee accounts work and short decimal strings,
        // separately from the reserved peak arithmetic working area above.
        budget.bytes(4096)?;
        if !value.is_finite() {
            return Err(Error::Unsupported("cannot encode non-finite float"));
        }
        let bits = value.to_bits();
        let negative = bits & !MAG != 0;
        if bits & MAG == 0 {
            return Ok(if negative { "-0.0" } else { "0.0" }.to_owned());
        }
        let c = self.select(bits & MAG)?;
        let digits = c.coefficient.to_string();
        let exponent = c.power + digits.len() as i32 - 1;
        let mut out = String::new();
        out.try_reserve_exact(24).map_err(|_| Error::Resource)?;
        if negative {
            out.push('-');
        }
        if (-4..17).contains(&exponent) {
            let point = digits.len() as i32 + c.power;
            if point <= 0 {
                out.push_str("0.");
                for _ in 0..-point {
                    out.push('0');
                }
                out.push_str(&digits);
            } else if point >= digits.len() as i32 {
                out.push_str(&digits);
                for _ in digits.len()..point as usize {
                    out.push('0');
                }
                out.push_str(".0");
            } else {
                let point = point as usize;
                out.push_str(&digits[..point]);
                out.push('.');
                out.push_str(&digits[point..]);
            }
        } else {
            out.push_str(&digits[..1]);
            if digits.len() > 1 {
                out.push('.');
                out.push_str(&digits[1..]);
            }
            out.push('e');
            out.push(if exponent < 0 { '-' } else { '+' });
            let e = exponent.unsigned_abs();
            if e < 10 {
                out.push('0');
            }
            out.push_str(&e.to_string());
        }
        if out.len() > 24 {
            return Err(Error::Internal("float output bound"));
        }
        Ok(out)
    }
}
