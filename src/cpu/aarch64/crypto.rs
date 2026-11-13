//! AArch64 CRC32 / CRC32C instructions and the Advanced SIMD cryptographic extensions
//! (AES, SHA-1, SHA-256), implemented from the architecture pseudocode.

use super::{Aarch64, Ex};
use std::sync::OnceLock;

/// CRC32 (`poly` = reflected 0xEDB88320) or CRC32C (reflected 0x82F63B78) over the low
/// `bytes` bytes of `data`, starting from `crc`.
pub fn crc32(crc: u32, data: u64, bytes: u32, castagnoli: bool) -> u32 {
    let poly: u32 = if castagnoli { 0x82f6_3b78 } else { 0xedb8_8320 };
    let mut c = crc;
    for k in 0..bytes {
        c ^= (data >> (8 * k)) as u8 as u32;
        for _ in 0..8 {
            c = if c & 1 == 1 { (c >> 1) ^ poly } else { c >> 1 };
        }
    }
    c
}

// ---------------------------------------------------------------- AES

fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0;
    while b != 0 {
        if b & 1 == 1 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    p
}

/// (S-box, inverse S-box), computed from the field inverse and the affine transform.
fn sboxes() -> &'static ([u8; 256], [u8; 256]) {
    static T: OnceLock<([u8; 256], [u8; 256])> = OnceLock::new();
    T.get_or_init(|| {
        let (mut s, mut inv) = ([0u8; 256], [0u8; 256]);
        for x in 0..256usize {
            // multiplicative inverse in GF(2^8) (0 maps to 0)
            let b = (1..=255u8).find(|&y| gmul(x as u8, y) == 1).unwrap_or(0);
            let mut r = b;
            for k in 1..=4 {
                r ^= b.rotate_left(k);
            }
            s[x] = r ^ 0x63;
            inv[s[x] as usize] = x as u8;
        }
        (s, inv)
    })
}

fn shift_rows(b: [u8; 16], inverse: bool) -> [u8; 16] {
    let mut o = [0u8; 16];
    for i in 0..16 {
        if inverse {
            o[i] = b[(i + 16 - 4 * (i % 4)) % 16];
        } else {
            o[i] = b[(i + 4 * (i % 4)) % 16];
        }
    }
    o
}

fn mix_columns(b: [u8; 16], inverse: bool) -> [u8; 16] {
    let m: [u8; 4] = if inverse { [14, 11, 13, 9] } else { [2, 3, 1, 1] };
    let mut o = [0u8; 16];
    for c in 0..4 {
        for r in 0..4 {
            let mut v = 0;
            for k in 0..4 {
                v ^= gmul(b[4 * c + k], m[(k + 4 - r) % 4]);
            }
            o[4 * c + r] = v;
        }
    }
    o
}

// ---------------------------------------------------------------- SHA

fn words(v: u128) -> [u32; 4] {
    [v as u32, (v >> 32) as u32, (v >> 64) as u32, (v >> 96) as u32]
}

fn from_words(w: [u32; 4]) -> u128 {
    w[0] as u128 | (w[1] as u128) << 32 | (w[2] as u128) << 64 | (w[3] as u128) << 96
}

fn sha1_hash(x: u128, y: u32, w: u128, op: u32) -> u128 {
    let (mut x, mut y) = (words(x), y);
    let w = words(w);
    for e in 0..4 {
        let (a, b, c, d) = (x[0], x[1], x[2], x[3]);
        let t = match op {
            0 => (b & c) | (!b & d),
            1 => b ^ c ^ d,
            _ => (b & c) | (b & d) | (c & d),
        };
        y = y.wrapping_add(a.rotate_left(5)).wrapping_add(t).wrapping_add(w[e]);
        x[1] = b.rotate_left(30);
        // <Y, X> = ROL(Y:X, 32)
        let (ny, nx) = (x[3], [y, x[0], x[1], x[2]]);
        y = ny;
        x = nx;
    }
    from_words(x)
}

fn big_sigma0(x: u32) -> u32 {
    x.rotate_right(2) ^ x.rotate_right(13) ^ x.rotate_right(22)
}
fn big_sigma1(x: u32) -> u32 {
    x.rotate_right(6) ^ x.rotate_right(11) ^ x.rotate_right(25)
}
fn small_sigma0(x: u32) -> u32 {
    x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3)
}
fn small_sigma1(x: u32) -> u32 {
    x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10)
}

fn sha256_hash(x: u128, y: u128, w: u128, part1: bool) -> u128 {
    let (mut x, mut y, w) = (words(x), words(y), words(w));
    for e in 0..4 {
        let chs = (y[0] & y[1]) ^ (!y[0] & y[2]);
        let maj = (x[0] & x[1]) ^ (x[0] & x[2]) ^ (x[1] & x[2]);
        let t = y[3].wrapping_add(big_sigma1(y[0])).wrapping_add(chs).wrapping_add(w[e]);
        x[3] = t.wrapping_add(x[3]);
        y[3] = t.wrapping_add(big_sigma0(x[0])).wrapping_add(maj);
        // <Y, X> = ROL(Y:X, 32)
        let (nx, ny) = ([y[3], x[0], x[1], x[2]], [x[3], y[0], y[1], y[2]]);
        x = nx;
        y = ny;
    }
    from_words(if part1 { x } else { y })
}

impl Aarch64 {
    /// CRC32{B,H,W,X} / CRC32C{B,H,W,X} (data-processing, 2 source).
    pub(super) fn crc32_insn(&mut self, i: u32, sf: bool) -> Result<u64, Ex> {
        let (opcode, rn, rm) = ((i >> 10) & 0x3f, ((i >> 5) & 31) as usize, ((i >> 16) & 31) as usize);
        let sz = opcode & 3;
        if (sz == 3) != sf {
            return Err(self.undef(i));
        }
        let bytes = 1u32 << sz;
        Ok(crc32(self.rz(rn) as u32, self.rz(rm), bytes, opcode & 4 != 0) as u64)
    }

    /// AESE / AESD / AESMC / AESIMC.
    pub(super) fn crypto_aes(&mut self, i: u32) -> Result<(), Ex> {
        let (rn, rd) = (((i >> 5) & 31) as usize, (i & 31) as usize);
        let (sbox, inv) = sboxes();
        let out = match (i >> 12) & 31 {
            4 | 5 => {
                let inverse = (i >> 12) & 1 == 1;
                let x = (self.vr[rd] ^ self.vr[rn]).to_le_bytes();
                let mut s = shift_rows(x, inverse);
                let t = if inverse { inv } else { sbox };
                s.iter_mut().for_each(|b| *b = t[*b as usize]);
                u128::from_le_bytes(s)
            }
            6 | 7 => u128::from_le_bytes(mix_columns(self.vr[rn].to_le_bytes(), (i >> 12) & 1 == 1)),
            _ => return Err(self.undef(i)),
        };
        self.vr[rd] = out;
        Ok(())
    }

    /// SHA1C/P/M/SU0, SHA256H/H2/SU1 (three-register) and SHA1H/SU1, SHA256SU0 (two-register).
    pub(super) fn crypto_sha(&mut self, i: u32) -> Result<(), Ex> {
        let (rm, rn, rd) = (((i >> 16) & 31) as usize, ((i >> 5) & 31) as usize, (i & 31) as usize);
        let (d, n, m) = (self.vr[rd], self.vr[rn], self.vr[rm]);
        if i & 0xffe0_8c00 == 0x5e00_0000 {
            self.vr[rd] = match (i >> 12) & 7 {
                0..=2 => sha1_hash(d, n as u32, m, (i >> 12) & 7),
                3 => {
                    // SHA1SU0: (Vn<63:0> : Vd<127:64>) ^ Vd ^ Vm
                    let t = (n & u64::MAX as u128) << 64 | d >> 64;
                    d ^ t ^ m
                }
                4 => sha256_hash(d, n, m, true),
                5 => sha256_hash(n, d, m, false),
                6 => {
                    // SHA256SU1
                    let (x, y, z) = (words(d), words(n), words(m));
                    let t0 = [y[1], y[2], y[3], z[0]]; // Z<31:0> : Y<127:32>
                    let mut r = [0u32; 4];
                    let mut t1 = [z[2], z[3]];
                    for e in 0..2 {
                        r[e] = small_sigma1(t1[e]).wrapping_add(x[e]).wrapping_add(t0[e]);
                    }
                    t1 = [r[0], r[1]];
                    for e in 2..4 {
                        r[e] = small_sigma1(t1[e - 2]).wrapping_add(x[e]).wrapping_add(t0[e]);
                    }
                    from_words(r)
                }
                _ => return Err(self.undef(i)),
            };
            return Ok(());
        }
        // two-register: SHA1H / SHA1SU1 / SHA256SU0
        self.vr[rd] = match (i >> 12) & 31 {
            0 => (n as u32).rotate_left(30) as u128,
            1 => {
                // SHA1SU1
                let (x, y) = (words(d), words(n));
                let t = [x[0] ^ y[1], x[1] ^ y[2], x[2] ^ y[3], x[3]];
                let w = [t[0].rotate_left(1), t[1].rotate_left(1), t[2].rotate_left(1), t[3].rotate_left(1) ^ t[0].rotate_left(2)];
                from_words(w)
            }
            2 => {
                // SHA256SU0
                let (x, y) = (words(d), words(n));
                let t = [x[1], x[2], x[3], y[0]];
                from_words([
                    x[0].wrapping_add(small_sigma0(t[0])),
                    x[1].wrapping_add(small_sigma0(t[1])),
                    x[2].wrapping_add(small_sigma0(t[2])),
                    x[3].wrapping_add(small_sigma0(t[3])),
                ])
            }
            _ => return Err(self.undef(i)),
        };
        Ok(())
    }
}
