//! Bit-exact IEEE 754 binary32/binary64 arithmetic with all rounding modes and
//! exception flags (RISC-V semantics: NaN results are the canonical NaN, tininess is
//! detected after rounding). Values are passed as raw bit patterns.
//!
//! Every operation works on an unpacked integer significand (`u128`) and an exponent,
//! computes the exact result (or the exact result with a sticky bit), and rounds once
//! in `round_pack`.

pub const NV: u8 = 16; // invalid
pub const DZ: u8 = 8; // divide by zero
pub const OF: u8 = 4; // overflow
pub const UF: u8 = 2; // underflow
pub const NX: u8 = 1; // inexact

/// Mode bit carried in the flags byte: detect tininess *before* rounding (ARM) instead of
/// after (RISC-V). Callers mask it out of the result with `& 0x1f`.
pub const TINY_BEFORE: u8 = 0x80;

pub const RNE: u8 = 0;
pub const RTZ: u8 = 1;
pub const RDN: u8 = 2;
pub const RUP: u8 = 3;
pub const RMM: u8 = 4;

#[derive(Clone, Copy)]
pub struct Fmt {
    exp: u32,
    frac: u32,
}
pub const F16: Fmt = Fmt { exp: 5, frac: 10 };
pub const F32: Fmt = Fmt { exp: 8, frac: 23 };
pub const F64: Fmt = Fmt { exp: 11, frac: 52 };

impl Fmt {
    fn bias(self) -> i32 {
        (1 << (self.exp - 1)) - 1
    }
    fn max_field(self) -> u64 {
        (1 << self.exp) - 1
    }
    fn frac_mask(self) -> u64 {
        (1 << self.frac) - 1
    }
    pub fn sign_bit(self) -> u64 {
        1 << (self.exp + self.frac)
    }
    pub fn canonical_nan(self) -> u64 {
        self.max_field() << self.frac | 1 << (self.frac - 1)
    }
    fn zero(self, sign: bool) -> u64 {
        if sign { self.sign_bit() } else { 0 }
    }
    fn inf(self, sign: bool) -> u64 {
        self.zero(sign) | self.max_field() << self.frac
    }
    fn max_finite(self, sign: bool) -> u64 {
        self.zero(sign) | (self.max_field() - 1) << self.frac | self.frac_mask()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum V {
    Zero,
    /// value = sig * 2^e
    Num(u128, i32),
    Inf,
    QNaN,
    SNaN,
}

fn unpack(f: Fmt, x: u64) -> (bool, V) {
    let sign = x & f.sign_bit() != 0;
    let field = (x >> f.frac) & f.max_field();
    let frac = x & f.frac_mask();
    let v = if field == f.max_field() {
        if frac == 0 {
            V::Inf
        } else if frac >> (f.frac - 1) & 1 == 1 {
            V::QNaN
        } else {
            V::SNaN
        }
    } else if field == 0 {
        if frac == 0 { V::Zero } else { V::Num(frac as u128, 1 - f.bias() - f.frac as i32) }
    } else {
        V::Num((frac | 1 << f.frac) as u128, field as i32 - f.bias() - f.frac as i32)
    };
    (sign, v)
}

fn is_nan(v: V) -> bool {
    matches!(v, V::QNaN | V::SNaN)
}

fn msb(x: u128) -> i32 {
    127 - x.leading_zeros() as i32
}

/// Shift right by `drop` bits: (kept bits, round bit, sticky).
fn shr_round(sig: u128, drop: i32) -> (u128, bool, bool) {
    if drop <= 0 {
        (sig << -drop, false, false)
    } else if drop > 128 {
        (0, false, sig != 0)
    } else if drop == 128 {
        (0, sig >> 127 == 1, sig << 1 != 0)
    } else {
        (sig >> drop, (sig >> (drop - 1)) & 1 == 1, sig & ((1u128 << (drop - 1)) - 1) != 0)
    }
}

fn round_inc(rm: u8, sign: bool, m: u128, rb: bool, st: bool) -> bool {
    match rm {
        RNE => rb && (st || m & 1 == 1),
        RTZ => false,
        RDN => sign && (rb || st),
        RUP => !sign && (rb || st),
        _ => rb, // RMM
    }
}

/// Round the exact value (-1)^sign * sig * 2^e to format `f`.
fn round_pack(f: Fmt, sign: bool, sig: u128, e: i32, rm: u8, fl: &mut u8) -> u64 {
    if sig == 0 {
        return f.zero(sign);
    }
    let p = f.frac as i32 + 1;
    let emin = 1 - f.bias();
    let top = msb(sig);
    let big_e = e + top; // exponent of the leading bit
    let keep = if big_e >= emin { p } else { p - (emin - big_e) };
    let (m, rb, st) = shr_round(sig, top + 1 - keep);
    let m = m + round_inc(rm, sign, m, rb, st) as u128;
    let inexact = rb || st;
    if big_e < emin && inexact {
        // tiny unless rounding to p bits with an unbounded exponent reaches 2^emin
        let tiny = *fl & TINY_BEFORE != 0 || big_e < emin - 1 || {
            let (m2, rb2, st2) = shr_round(sig, top + 1 - p);
            m2 + (round_inc(rm, sign, m2, rb2, st2) as u128) < 1u128 << p
        };
        if tiny {
            *fl |= UF;
        }
    }
    if inexact {
        *fl |= NX;
    }
    // normal: ((E + bias - 1) << frac) + m, where m carries the hidden bit (a carry
    // out of m bumps the exponent); subnormal: just m (m = 2^frac becomes min normal)
    let base = if big_e >= emin { (big_e + f.bias() - 1) as u128 } else { 0 };
    let bits = (base << f.frac) + m;
    if bits >= (f.max_field() as u128) << f.frac {
        *fl |= OF | NX;
        let to_inf = match rm {
            RNE | RMM => true,
            RTZ => false,
            RDN => sign,
            _ => !sign,
        };
        return if to_inf { f.inf(sign) } else { f.max_finite(sign) };
    }
    f.zero(sign) | bits as u64
}

/// NaN propagation for two operands: Some(canonical NaN) if either is a NaN.
fn nan2(f: Fmt, a: V, b: V, fl: &mut u8) -> Option<u64> {
    if a == V::SNaN || b == V::SNaN {
        *fl |= NV;
    }
    (is_nan(a) || is_nan(b)).then(|| f.canonical_nan())
}

fn sticky_shr(x: u128, d: i32) -> u128 {
    if d == 0 {
        x
    } else if d >= 127 {
        (x != 0) as u128
    } else {
        (x >> d) | ((x & ((1u128 << d) - 1)) != 0) as u128
    }
}

/// Exact-with-sticky sum of two signed numbers; both significands < 2^126.
fn add_exact(sa: bool, ma: u128, ea: i32, sb: bool, mb: u128, eb: i32) -> (bool, u128, i32) {
    let (na, nb) = (125 - msb(ma), 125 - msb(mb));
    let (mut ma, mut ea, mut mb, mut eb, mut sa, mut sb) = (ma << na, ea - na, mb << nb, eb - nb, sa, sb);
    if ea < eb {
        std::mem::swap(&mut ma, &mut mb);
        std::mem::swap(&mut ea, &mut eb);
        std::mem::swap(&mut sa, &mut sb);
    }
    let mb = sticky_shr(mb, ea - eb);
    if sa == sb {
        (sa, ma + mb, ea)
    } else if ma >= mb {
        (sa, ma - mb, ea)
    } else {
        (sb, mb - ma, ea)
    }
}

pub fn add(f: Fmt, a: u64, b: u64, rm: u8, fl: &mut u8) -> u64 {
    let ((sa, va), (sb, vb)) = (unpack(f, a), unpack(f, b));
    if let Some(n) = nan2(f, va, vb, fl) {
        return n;
    }
    match (va, vb) {
        (V::Inf, V::Inf) if sa != sb => {
            *fl |= NV;
            f.canonical_nan()
        }
        (V::Inf, _) => f.inf(sa),
        (_, V::Inf) => f.inf(sb),
        (V::Zero, V::Zero) => f.zero(if sa == sb { sa } else { rm == RDN }),
        (V::Zero, _) => b,
        (_, V::Zero) => a,
        (V::Num(ma, ea), V::Num(mb, eb)) => {
            let (s, m, e) = add_exact(sa, ma, ea, sb, mb, eb);
            if m == 0 { f.zero(rm == RDN) } else { round_pack(f, s, m, e, rm, fl) }
        }
        _ => unreachable!(),
    }
}

pub fn sub(f: Fmt, a: u64, b: u64, rm: u8, fl: &mut u8) -> u64 {
    // negating a NaN is harmless: NaN results are canonical anyway
    add(f, a, b ^ f.sign_bit(), rm, fl)
}

pub fn mul(f: Fmt, a: u64, b: u64, rm: u8, fl: &mut u8) -> u64 {
    let ((sa, va), (sb, vb)) = (unpack(f, a), unpack(f, b));
    if let Some(n) = nan2(f, va, vb, fl) {
        return n;
    }
    let s = sa ^ sb;
    match (va, vb) {
        (V::Inf, V::Zero) | (V::Zero, V::Inf) => {
            *fl |= NV;
            f.canonical_nan()
        }
        (V::Inf, _) | (_, V::Inf) => f.inf(s),
        (V::Zero, _) | (_, V::Zero) => f.zero(s),
        (V::Num(ma, ea), V::Num(mb, eb)) => round_pack(f, s, ma * mb, ea + eb, rm, fl),
        _ => unreachable!(),
    }
}

pub fn div(f: Fmt, a: u64, b: u64, rm: u8, fl: &mut u8) -> u64 {
    let ((sa, va), (sb, vb)) = (unpack(f, a), unpack(f, b));
    if let Some(n) = nan2(f, va, vb, fl) {
        return n;
    }
    let s = sa ^ sb;
    match (va, vb) {
        (V::Inf, V::Inf) | (V::Zero, V::Zero) => {
            *fl |= NV;
            f.canonical_nan()
        }
        (V::Inf, _) => f.inf(s),
        (_, V::Inf) => f.zero(s),
        (V::Zero, _) => f.zero(s),
        (_, V::Zero) => {
            *fl |= DZ;
            f.inf(s)
        }
        (V::Num(ma, ea), V::Num(mb, eb)) => {
            let (ka, kb) = (126 - msb(ma), 63 - msb(mb));
            let (na, nb) = (ma << ka, mb << kb);
            let q = (na / nb) << 1 | (na % nb != 0) as u128;
            round_pack(f, s, q, (ea - ka) - (eb - kb) - 1, rm, fl)
        }
        _ => unreachable!(),
    }
}

fn isqrt(n: u128) -> u128 {
    let mut x = (n as f64).sqrt() as u128;
    for _ in 0..2 {
        if x > 0 {
            x = (x + n / x) / 2;
        }
    }
    while x * x > n {
        x -= 1;
    }
    while (x + 1) * (x + 1) <= n {
        x += 1;
    }
    x
}

pub fn sqrt(f: Fmt, a: u64, rm: u8, fl: &mut u8) -> u64 {
    let (s, v) = unpack(f, a);
    match v {
        V::SNaN | V::QNaN => {
            if v == V::SNaN {
                *fl |= NV;
            }
            f.canonical_nan()
        }
        V::Zero => a,
        _ if s => {
            *fl |= NV;
            f.canonical_nan()
        }
        V::Inf => a,
        V::Num(m, e) => {
            let mut k = 124 - msb(m);
            if (e - k) & 1 != 0 {
                k += 1;
            }
            let n = m << k;
            let r = isqrt(n);
            let sig = r << 1 | (r * r != n) as u128;
            round_pack(f, false, sig, (e - k) / 2 - 1, rm, fl)
        }
    }
}

/// a * b + c with a single rounding.
pub fn fma(f: Fmt, a: u64, b: u64, c: u64, rm: u8, fl: &mut u8) -> u64 {
    fma_scaled(f, a, b, c, 0, rm, fl)
}

/// (a * b + c) * 2^scale with a single rounding (FRSQRTS uses scale = -1).
pub fn fma_scaled(f: Fmt, a: u64, b: u64, c: u64, scale: i32, rm: u8, fl: &mut u8) -> u64 {
    let ((sa, va), (sb, vb), (sc, vc)) = (unpack(f, a), unpack(f, b), unpack(f, c));
    let inf_zero = matches!((va, vb), (V::Inf, V::Zero) | (V::Zero, V::Inf));
    if va == V::SNaN || vb == V::SNaN || vc == V::SNaN || inf_zero {
        *fl |= NV;
    }
    if is_nan(va) || is_nan(vb) || is_nan(vc) || inf_zero {
        return f.canonical_nan();
    }
    let sp = sa ^ sb;
    match (va, vb, vc) {
        (V::Inf, _, _) | (_, V::Inf, _) => {
            if vc == V::Inf && sc != sp {
                *fl |= NV;
                f.canonical_nan()
            } else {
                f.inf(sp)
            }
        }
        (_, _, V::Inf) => f.inf(sc),
        (V::Zero, _, V::Zero) | (_, V::Zero, V::Zero) => f.zero(if sp == sc { sp } else { rm == RDN }),
        (V::Zero, _, V::Num(mc, ec)) | (_, V::Zero, V::Num(mc, ec)) => round_pack(f, sc, mc, ec + scale, rm, fl),
        (V::Num(ma, ea), V::Num(mb, eb), V::Zero) => round_pack(f, sp, ma * mb, ea + eb + scale, rm, fl),
        (V::Num(ma, ea), V::Num(mb, eb), V::Num(mc, ec)) => {
            let (s, m, e) = add_exact(sp, ma * mb, ea + eb, sc, mc, ec);
            if m == 0 { f.zero(rm == RDN) } else { round_pack(f, s, m, e + scale, rm, fl) }
        }
        _ => unreachable!(),
    }
}

/// Ordering key for non-NaN values; `signed_zero` orders -0 below +0.
fn key(f: Fmt, x: u64, signed_zero: bool) -> i128 {
    let mag = (x & !f.sign_bit()) as i128;
    if x & f.sign_bit() == 0 {
        mag
    } else if signed_zero {
        -mag - 1
    } else {
        -mag
    }
}

pub fn eq(f: Fmt, a: u64, b: u64, fl: &mut u8) -> bool {
    let (va, vb) = (unpack(f, a).1, unpack(f, b).1);
    if va == V::SNaN || vb == V::SNaN {
        *fl |= NV;
    }
    !is_nan(va) && !is_nan(vb) && key(f, a, false) == key(f, b, false)
}

/// Signalling comparisons (flt / fle): any NaN raises NV.
pub fn lt(f: Fmt, a: u64, b: u64, fl: &mut u8) -> bool {
    if is_nan(unpack(f, a).1) || is_nan(unpack(f, b).1) {
        *fl |= NV;
        return false;
    }
    key(f, a, false) < key(f, b, false)
}

pub fn le(f: Fmt, a: u64, b: u64, fl: &mut u8) -> bool {
    if is_nan(unpack(f, a).1) || is_nan(unpack(f, b).1) {
        *fl |= NV;
        return false;
    }
    key(f, a, false) <= key(f, b, false)
}

/// IEEE 754-2008 minNum / maxNum as in RISC-V F 2.2 (a quiet NaN loses; -0 < +0).
pub fn min_max(f: Fmt, a: u64, b: u64, max: bool, fl: &mut u8) -> u64 {
    let (va, vb) = (unpack(f, a).1, unpack(f, b).1);
    if va == V::SNaN || vb == V::SNaN {
        *fl |= NV;
    }
    match (is_nan(va), is_nan(vb)) {
        (true, true) => f.canonical_nan(),
        (true, false) => b,
        (false, true) => a,
        _ => {
            let a_less = key(f, a, true) < key(f, b, true);
            if a_less != max { a } else { b }
        }
    }
}

pub fn is_snan(f: Fmt, x: u64) -> bool {
    unpack(f, x).1 == V::SNaN
}

pub fn is_qnan(f: Fmt, x: u64) -> bool {
    unpack(f, x).1 == V::QNaN
}

pub fn is_nan_bits(f: Fmt, x: u64) -> bool {
    is_nan(unpack(f, x).1)
}

pub fn is_inf(f: Fmt, x: u64) -> bool {
    unpack(f, x).1 == V::Inf
}

pub fn is_zero(f: Fmt, x: u64) -> bool {
    unpack(f, x).1 == V::Zero
}

/// The quiet version of a NaN (sets the top fraction bit).
pub fn quiet(f: Fmt, x: u64) -> u64 {
    x | 1 << (f.frac - 1)
}

pub fn default_nan(f: Fmt) -> u64 {
    f.canonical_nan()
}

pub fn neg(f: Fmt, x: u64) -> u64 {
    x ^ f.sign_bit()
}

pub fn abs(f: Fmt, x: u64) -> u64 {
    x & !f.sign_bit()
}

/// Total order of two non-NaN values (-0 == +0): Less / Equal / Greater.
pub fn compare(f: Fmt, a: u64, b: u64) -> std::cmp::Ordering {
    key(f, a, false).cmp(&key(f, b, false))
}

pub fn classify(f: Fmt, x: u64) -> u64 {
    let (s, v) = unpack(f, x);
    let subnormal = (x >> f.frac) & f.max_field() == 0;
    1 << match (v, s) {
        (V::Inf, true) => 0,
        (V::Num(..), true) if !subnormal => 1,
        (V::Num(..), true) => 2,
        (V::Zero, true) => 3,
        (V::Zero, false) => 4,
        (V::Num(..), false) if subnormal => 5,
        (V::Num(..), false) => 6,
        (V::Inf, false) => 7,
        (V::SNaN, _) => 8,
        (V::QNaN, _) => 9,
    }
}

/// Float -> integer (`bits` = 32 or 64), saturating; the result is sign-extended
/// to 64 bits (RISC-V puts 32-bit results sign-extended even when unsigned).
pub fn to_int(f: Fmt, x: u64, rm: u8, signed: bool, bits: u32, fl: &mut u8) -> u64 {
    to_int_scaled(f, x, rm, signed, bits, 0, fl)
}

/// Like `to_int` but converts `x * 2^scale` (fixed-point conversions).
pub fn to_int_scaled(f: Fmt, x: u64, rm: u8, signed: bool, bits: u32, scale: i32, fl: &mut u8) -> u64 {
    let (s, v) = unpack(f, x);
    let v = match v {
        V::Num(m, e) => V::Num(m, e + scale),
        v => v,
    };
    let max: i128 = if signed { (1i128 << (bits - 1)) - 1 } else { (1i128 << bits) - 1 };
    let min: i128 = if signed { -(1i128 << (bits - 1)) } else { 0 };
    let ext = |r: i128| if bits == 32 { r as i32 as i64 as u64 } else { r as u64 };
    let (mag, inexact) = match v {
        V::QNaN | V::SNaN => {
            *fl |= NV;
            return ext(max);
        }
        V::Inf => {
            *fl |= NV;
            return ext(if s { min } else { max });
        }
        V::Zero => (0, false),
        V::Num(m, e) => {
            if e >= 0 {
                if e > 70 { (i128::MAX, false) } else { ((m << e) as i128, false) }
            } else {
                let (q, rb, st) = shr_round(m, -e);
                ((q + round_inc(rm, s, q, rb, st) as u128) as i128, rb || st)
            }
        }
    };
    let r = if s { -mag } else { mag };
    if r > max || r < min {
        *fl |= NV;
        return ext(if r > max { max } else { min });
    }
    if inexact {
        *fl |= NX;
    }
    ext(r)
}

/// Integer -> float. `v` is the source register; `bits` = 32 uses its low word.
pub fn from_int(f: Fmt, v: u64, signed: bool, bits: u32, rm: u8, fl: &mut u8) -> u64 {
    from_int_scaled(f, v, signed, bits, 0, rm, fl)
}

/// Integer -> float of `v * 2^scale` (fixed-point conversions use a negative scale).
pub fn from_int_scaled(f: Fmt, v: u64, signed: bool, bits: u32, scale: i32, rm: u8, fl: &mut u8) -> u64 {
    let x: i128 = match (bits, signed) {
        (32, true) => v as i32 as i128,
        (32, false) => v as u32 as i128,
        (_, true) => v as i64 as i128,
        _ => v as i128,
    };
    round_pack(f, x < 0, x.unsigned_abs(), scale, rm, fl)
}

/// Round to an integral value in the same format (FRINT*). `exact` raises Inexact.
pub fn round_to_int(f: Fmt, x: u64, rm: u8, exact: bool, fl: &mut u8) -> u64 {
    let (s, v) = unpack(f, x);
    match v {
        V::Num(m, e) if e < 0 => {
            let (q, rb, st) = shr_round(m, -e);
            let q = q + round_inc(rm, s, q, rb, st) as u128;
            if exact && (rb || st) {
                *fl |= NX;
            }
            let mut dummy = 0;
            if q == 0 { f.zero(s) } else { round_pack(f, s, q, 0, RNE, &mut dummy) }
        }
        _ => x, // zero, infinity, already integral (NaNs are handled by the caller)
    }
}

/// Convert a NaN to another format keeping the sign and as much payload as fits (quieted).
pub fn convert_nan(from: Fmt, to: Fmt, x: u64) -> u64 {
    let sign = x & from.sign_bit() != 0;
    let frac = x & from.frac_mask();
    let frac = if to.frac >= from.frac { frac << (to.frac - from.frac) } else { frac >> (from.frac - to.frac) };
    to.zero(sign) | to.max_field() << to.frac | frac | 1 << (to.frac - 1)
}

/// Convert between formats (fcvt.s.d / fcvt.d.s).
pub fn convert(from: Fmt, to: Fmt, x: u64, rm: u8, fl: &mut u8) -> u64 {
    let (s, v) = unpack(from, x);
    match v {
        V::SNaN | V::QNaN => {
            if v == V::SNaN {
                *fl |= NV;
            }
            to.canonical_nan()
        }
        V::Inf => to.inf(s),
        V::Zero => to.zero(s),
        V::Num(m, e) => round_pack(to, s, m, e, rm, fl),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(x: f64) -> u64 {
        x.to_bits()
    }
    fn s(x: f32) -> u64 {
        x.to_bits() as u64
    }

    #[test]
    fn matches_host_round_to_nearest() {
        let vals = [0.0, -0.0, 1.0, -1.5, 3.25, 1e-310, -2.5e-320, 1e308, 7.0, 0.1, 1.0 / 3.0, f64::MAX, f64::MIN_POSITIVE];
        for &a in &vals {
            for &b in &vals {
                let mut fl = 0;
                assert_eq!(add(F64, d(a), d(b), RNE, &mut fl), d(a + b), "{a} + {b}");
                assert_eq!(mul(F64, d(a), d(b), RNE, &mut fl), d(a * b), "{a} * {b}");
                if b != 0.0 {
                    assert_eq!(div(F64, d(a), d(b), RNE, &mut fl), d(a / b), "{a} / {b}");
                }
                assert_eq!(fma(F64, d(a), d(b), d(0.5), RNE, &mut fl), d(a.mul_add(b, 0.5)), "fma {a} {b}");
                let (fa, fb) = (a as f32, b as f32);
                assert_eq!(add(F32, s(fa), s(fb), RNE, &mut fl), s(fa + fb));
                assert_eq!(mul(F32, s(fa), s(fb), RNE, &mut fl), s(fa * fb));
            }
            if a >= 0.0 {
                let mut fl = 0;
                assert_eq!(sqrt(F64, d(a), RNE, &mut fl), d(a.sqrt()), "sqrt {a}");
            }
            let mut fl = 0;
            assert_eq!(convert(F64, F32, d(a), RNE, &mut fl), s(a as f32), "cvt {a}");
        }
    }

    #[test]
    fn random_bits_match_host() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            // bias towards interesting exponents: keep sign/frac random, squeeze the exponent
            match x % 4 {
                0 => x,
                1 => (x & 0x800f_ffff_ffff_ffff) | ((0x3f0 + (x >> 52) % 32) << 52),
                2 => x & 0x800f_ffff_ffff_ffff, // subnormal
                _ => (x & 0x800f_ffff_ffff_ffff) | ((0x7fd + (x >> 52) % 2) << 52),
            }
        };
        let same = |got: u64, want: f64, what: &str| {
            if want.is_nan() {
                assert_eq!(got, F64.canonical_nan(), "{what}");
            } else {
                assert_eq!(got, want.to_bits(), "{what}");
            }
        };
        let same32 = |got: u64, want: f32, what: &str| {
            if want.is_nan() {
                assert_eq!(got, F32.canonical_nan(), "{what}");
            } else {
                assert_eq!(got, want.to_bits() as u64, "{what}");
            }
        };
        for _ in 0..200_000 {
            let (a, b, c) = (next(), next(), next());
            let (fa, fb, fc) = (f64::from_bits(a), f64::from_bits(b), f64::from_bits(c));
            let mut fl = 0;
            same(add(F64, a, b, RNE, &mut fl), fa + fb, &format!("add {a:#x} {b:#x}"));
            same(mul(F64, a, b, RNE, &mut fl), fa * fb, &format!("mul {a:#x} {b:#x}"));
            same(div(F64, a, b, RNE, &mut fl), fa / fb, &format!("div {a:#x} {b:#x}"));
            same(sqrt(F64, a, RNE, &mut fl), fa.sqrt(), &format!("sqrt {a:#x}"));
            same(fma(F64, a, b, c, RNE, &mut fl), fa.mul_add(fb, fc), &format!("fma {a:#x} {b:#x} {c:#x}"));
            let (sa, sb, sc) = (a as u32 as u64, b as u32 as u64, c as u32 as u64);
            let (ga, gb, gc) = (f32::from_bits(sa as u32), f32::from_bits(sb as u32), f32::from_bits(sc as u32));
            same32(add(F32, sa, sb, RNE, &mut fl), ga + gb, "add32");
            same32(mul(F32, sa, sb, RNE, &mut fl), ga * gb, "mul32");
            same32(div(F32, sa, sb, RNE, &mut fl), ga / gb, "div32");
            same32(sqrt(F32, sa, RNE, &mut fl), ga.sqrt(), "sqrt32");
            same32(fma(F32, sa, sb, sc, RNE, &mut fl), ga.mul_add(gb, gc), "fma32");
            same32(convert(F64, F32, a, RNE, &mut fl), fa as f32, &format!("cvt {a:#x}"));
        }
    }

    #[test]
    fn flags_and_rounding_modes() {
        let mut fl = 0;
        assert_eq!(add(F64, d(1.0), d(2.0), RNE, &mut fl), d(3.0));
        assert_eq!(fl, 0);
        div(F64, d(1.0), d(3.0), RNE, &mut fl);
        assert_eq!(fl, NX);
        fl = 0;
        assert_eq!(div(F64, d(1.0), d(0.0), RNE, &mut fl), d(f64::INFINITY));
        assert_eq!(fl, DZ);
        fl = 0;
        assert_eq!(mul(F64, d(f64::MAX), d(2.0), RTZ, &mut fl), d(f64::MAX));
        assert_eq!(fl, OF | NX);
        fl = 0;
        assert_eq!(sqrt(F64, d(-1.0), RNE, &mut fl), F64.canonical_nan());
        assert_eq!(fl, NV);
        // 1/3 rounded down vs up differ by one ulp
        let (mut a, mut b) = (0, 0);
        assert_eq!(div(F64, d(1.0), d(3.0), RUP, &mut a) - div(F64, d(1.0), d(3.0), RDN, &mut b), 1);
        fl = 0;
        assert_eq!(to_int(F64, d(-1.5), RNE, true, 32, &mut fl) as i64, -2);
        assert_eq!(to_int(F64, d(2.5), RNE, true, 64, &mut fl), 2);
        assert_eq!(to_int(F64, d(2.5), RMM, true, 64, &mut fl), 3);
        assert_eq!(to_int(F64, d(-3.0), RNE, false, 64, &mut fl), 0);
        assert_eq!(to_int(F64, d(1e20), RNE, true, 32, &mut fl), 0x7fff_ffff);
        assert_eq!(from_int(F32, u64::MAX, false, 64, RNE, &mut fl), s(1.8446744e19));
        fl = 0;
        // subnormal result, inexact -> underflow
        mul(F64, d(1e-200), d(1e-120), RNE, &mut fl);
        assert_eq!(fl, UF | NX);
    }
}
