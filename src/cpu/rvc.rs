//! RV64C: expand a 16-bit compressed instruction into its 32-bit equivalent.
//! Returns `None` for illegal/reserved encodings (and for C.F* since F/D are absent).

#[inline]
fn bit(c: u32, n: u32) -> u32 {
    (c >> n) & 1
}

/// Sign-extend the low `bits` bits of `v`.
fn sx(v: u32, bits: u32) -> i32 {
    ((v << (32 - bits)) as i32) >> (32 - bits)
}

fn r(op: u32, rd: u32, f3: u32, rs1: u32, rs2: u32, f7: u32) -> u32 {
    f7 << 25 | rs2 << 20 | rs1 << 15 | f3 << 12 | rd << 7 | op
}
fn i(op: u32, rd: u32, f3: u32, rs1: u32, imm: i32) -> u32 {
    ((imm as u32) & 0xfff) << 20 | rs1 << 15 | f3 << 12 | rd << 7 | op
}
fn s(f3: u32, rs1: u32, rs2: u32, imm: i32) -> u32 {
    let m = imm as u32;
    (m >> 5 & 0x7f) << 25 | rs2 << 20 | rs1 << 15 | f3 << 12 | (m & 0x1f) << 7 | 0x23
}
fn b(f3: u32, rs1: u32, rs2: u32, imm: i32) -> u32 {
    let m = imm as u32;
    (m >> 12 & 1) << 31 | (m >> 5 & 0x3f) << 25 | rs2 << 20 | rs1 << 15 | f3 << 12 | (m >> 1 & 0xf) << 8 | (m >> 11 & 1) << 7 | 0x63
}
fn jal(rd: u32, imm: i32) -> u32 {
    let m = imm as u32;
    (m >> 20 & 1) << 31 | (m >> 1 & 0x3ff) << 21 | (m >> 11 & 1) << 20 | (m >> 12 & 0xff) << 12 | rd << 7 | 0x6f
}

/// Table-driven `expand`: all 2^16 encodings are expanded once (256 KiB);
/// 0 marks an illegal encoding (0 is not a valid 32-bit instruction either).
#[inline]
pub fn expand_fast(c: u16) -> Option<u32> {
    use std::sync::OnceLock;
    static TABLE: OnceLock<Box<[u32]>> = OnceLock::new();
    let t = TABLE.get_or_init(|| (0..=u16::MAX).map(|c| expand(c).unwrap_or(0)).collect());
    match t[c as usize] {
        0 => None,
        i => Some(i),
    }
}

pub fn expand(c: u16) -> Option<u32> {
    let c = c as u32;
    let f3 = c >> 13;
    let rd = (c >> 7) & 31; // also rs1
    let rs2 = (c >> 2) & 31;
    let rdp = 8 + ((c >> 2) & 7); // rd' / rs2'
    let rs1p = 8 + ((c >> 7) & 7);
    let imm6 = sx(bit(c, 12) << 5 | (c >> 2) & 31, 6);

    Some(match (c & 3, f3) {
        // ---------------- quadrant 0
        (0, 0) => {
            // C.ADDI4SPN
            let imm = bit(c, 6) << 2 | bit(c, 5) << 3 | (c >> 11 & 3) << 4 | (c >> 7 & 0xf) << 6;
            if imm == 0 {
                return None;
            }
            i(0x13, rdp, 0, 2, imm as i32)
        }
        (0, 2) => i(0x03, rdp, 2, rs1p, ((c >> 10 & 7) << 3 | bit(c, 6) << 2 | bit(c, 5) << 6) as i32), // C.LW
        (0, 3) => i(0x03, rdp, 3, rs1p, ((c >> 10 & 7) << 3 | (c >> 5 & 3) << 6) as i32),             // C.LD
        (0, 6) => s(2, rs1p, rdp, ((c >> 10 & 7) << 3 | bit(c, 6) << 2 | bit(c, 5) << 6) as i32),      // C.SW
        (0, 7) => s(3, rs1p, rdp, ((c >> 10 & 7) << 3 | (c >> 5 & 3) << 6) as i32),                   // C.SD

        // ---------------- quadrant 1
        (1, 0) => i(0x13, rd, 0, rd, imm6), // C.ADDI / C.NOP
        (1, 1) => {
            if rd == 0 {
                return None;
            }
            i(0x1b, rd, 0, rd, imm6) // C.ADDIW
        }
        (1, 2) => i(0x13, rd, 0, 0, imm6), // C.LI
        (1, 3) if rd == 2 => {
            // C.ADDI16SP
            let imm = sx(bit(c, 12) << 9 | bit(c, 6) << 4 | bit(c, 5) << 6 | (c >> 3 & 3) << 7 | bit(c, 2) << 5, 10);
            if imm == 0 {
                return None;
            }
            i(0x13, 2, 0, 2, imm)
        }
        (1, 3) => {
            // C.LUI
            if imm6 == 0 {
                return None;
            }
            ((imm6 as u32) << 12) | rd << 7 | 0x37
        }
        (1, 4) => {
            let sh = bit(c, 12) << 5 | (c >> 2) & 31;
            match c >> 10 & 3 {
                0 => i(0x13, rs1p, 5, rs1p, sh as i32),          // C.SRLI
                1 => i(0x13, rs1p, 5, rs1p, (sh | 0x400) as i32), // C.SRAI
                2 => i(0x13, rs1p, 7, rs1p, imm6),               // C.ANDI
                _ => match (bit(c, 12), c >> 5 & 3) {
                    (0, 0) => r(0x33, rs1p, 0, rs1p, rdp, 0x20), // C.SUB
                    (0, 1) => r(0x33, rs1p, 4, rs1p, rdp, 0),    // C.XOR
                    (0, 2) => r(0x33, rs1p, 6, rs1p, rdp, 0),    // C.OR
                    (0, 3) => r(0x33, rs1p, 7, rs1p, rdp, 0),    // C.AND
                    (1, 0) => r(0x3b, rs1p, 0, rs1p, rdp, 0x20), // C.SUBW
                    (1, 1) => r(0x3b, rs1p, 0, rs1p, rdp, 0),    // C.ADDW
                    _ => return None,
                },
            }
        }
        (1, 5) => {
            // C.J
            let imm = bit(c, 12) << 11 | bit(c, 11) << 4 | (c >> 9 & 3) << 8 | bit(c, 8) << 10 | bit(c, 7) << 6
                | bit(c, 6) << 7 | (c >> 3 & 7) << 1 | bit(c, 2) << 5;
            jal(0, sx(imm, 12))
        }
        (1, 6) | (1, 7) => {
            // C.BEQZ / C.BNEZ
            let imm = bit(c, 12) << 8 | (c >> 10 & 3) << 3 | (c >> 5 & 3) << 6 | (c >> 3 & 3) << 1 | bit(c, 2) << 5;
            b(f3 - 6, rs1p, 0, sx(imm, 9))
        }

        // ---------------- quadrant 2
        (2, 0) => i(0x13, rd, 1, rd, (bit(c, 12) << 5 | (c >> 2) & 31) as i32), // C.SLLI
        (2, 2) => {
            if rd == 0 {
                return None;
            }
            i(0x03, rd, 2, 2, (bit(c, 12) << 5 | (c >> 4 & 7) << 2 | (c >> 2 & 3) << 6) as i32) // C.LWSP
        }
        (2, 3) => {
            if rd == 0 {
                return None;
            }
            i(0x03, rd, 3, 2, (bit(c, 12) << 5 | (c >> 5 & 3) << 3 | (c >> 2 & 7) << 6) as i32) // C.LDSP
        }
        (2, 4) => match (bit(c, 12), rd, rs2) {
            (0, 0, 0) => return None,
            (0, _, 0) => i(0x67, 0, 0, rd, 0),     // C.JR
            (0, _, _) => r(0x33, rd, 0, 0, rs2, 0), // C.MV
            (1, 0, 0) => 0x0010_0073,               // C.EBREAK
            (1, _, 0) => i(0x67, 1, 0, rd, 0),     // C.JALR
            _ => r(0x33, rd, 0, rd, rs2, 0),        // C.ADD
        },
        (2, 6) => s(2, 2, rs2, ((c >> 9 & 0xf) << 2 | (c >> 7 & 3) << 6) as i32), // C.SWSP
        (2, 7) => s(3, 2, rs2, ((c >> 10 & 7) << 3 | (c >> 7 & 7) << 6) as i32), // C.SDSP
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::expand;

    #[test]
    fn known_encodings() {
        assert_eq!(expand(0x0001), Some(0x0000_0013)); // c.nop -> addi x0,x0,0
        assert_eq!(expand(0x4505), Some(0x0010_0513)); // c.li a0,1
        assert_eq!(expand(0x8082), Some(0x0000_8067)); // c.ret -> jalr x0,0(ra)
        assert_eq!(expand(0x1141), Some(0xff01_0113)); // c.addi sp,sp,-16
        assert_eq!(expand(0xe406), Some(0x0011_3423)); // c.sdsp ra,8(sp)
        assert_eq!(expand(0x60a2), Some(0x0081_3083)); // c.ldsp ra,8(sp)
        assert_eq!(expand(0x0000), None);
    }
}
