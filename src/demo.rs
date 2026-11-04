//! Tiny hand-rolled encoders + demo programs, so ruvm can be tried without a
//! cross toolchain.

/// RISC-V instruction encoders (only what the demos need).
pub mod rv {
    pub fn r(op: u32, rd: u32, f3: u32, rs1: u32, rs2: u32, f7: u32) -> u32 {
        f7 << 25 | rs2 << 20 | rs1 << 15 | f3 << 12 | rd << 7 | op
    }
    pub fn i(op: u32, rd: u32, f3: u32, rs1: u32, imm: i32) -> u32 {
        ((imm as u32) & 0xfff) << 20 | rs1 << 15 | f3 << 12 | rd << 7 | op
    }
    pub fn s(f3: u32, rs1: u32, rs2: u32, imm: i32) -> u32 {
        let u = imm as u32;
        ((u >> 5) & 0x7f) << 25 | rs2 << 20 | rs1 << 15 | f3 << 12 | (u & 0x1f) << 7 | 0x23
    }
    pub fn b(f3: u32, rs1: u32, rs2: u32, imm: i32) -> u32 {
        let u = imm as u32;
        ((u >> 12) & 1) << 31 | ((u >> 5) & 0x3f) << 25 | rs2 << 20 | rs1 << 15 | f3 << 12
            | ((u >> 1) & 0xf) << 8 | ((u >> 11) & 1) << 7 | 0x63
    }
    pub fn u(op: u32, rd: u32, imm20: u32) -> u32 {
        imm20 << 12 | rd << 7 | op
    }
    pub fn jal(rd: u32, imm: i32) -> u32 {
        let u = imm as u32;
        ((u >> 20) & 1) << 31 | ((u >> 1) & 0x3ff) << 21 | ((u >> 11) & 1) << 20
            | ((u >> 12) & 0xff) << 12 | rd << 7 | 0x6f
    }
    pub fn lui(rd: u32, imm20: u32) -> u32 { u(0x37, rd, imm20) }
    pub fn auipc(rd: u32, imm20: u32) -> u32 { u(0x17, rd, imm20) }
    pub fn addi(rd: u32, rs1: u32, imm: i32) -> u32 { i(0x13, rd, 0, rs1, imm) }
    pub fn add(rd: u32, rs1: u32, rs2: u32) -> u32 { r(0x33, rd, 0, rs1, rs2, 0) }
    pub fn mul(rd: u32, rs1: u32, rs2: u32) -> u32 { r(0x33, rd, 0, rs1, rs2, 1) }
    pub fn lbu(rd: u32, rs1: u32, imm: i32) -> u32 { i(0x03, rd, 4, rs1, imm) }
    pub fn sb(rs1: u32, rs2: u32, imm: i32) -> u32 { s(0, rs1, rs2, imm) }
    pub fn sw(rs1: u32, rs2: u32, imm: i32) -> u32 { s(2, rs1, rs2, imm) }
    pub fn beq(rs1: u32, rs2: u32, imm: i32) -> u32 { b(0, rs1, rs2, imm) }
    pub fn bne(rs1: u32, rs2: u32, imm: i32) -> u32 { b(1, rs1, rs2, imm) }
    pub fn ecall() -> u32 { 0x73 }
    pub fn mret() -> u32 { 0x3020_0073 }
    pub fn slli(rd: u32, rs1: u32, sh: u32) -> u32 { i(0x13, rd, 1, rs1, sh as i32) }
    pub fn or(rd: u32, rs1: u32, rs2: u32) -> u32 { r(0x33, rd, 6, rs1, rs2, 0) }
    pub fn ld(rd: u32, rs1: u32, imm: i32) -> u32 { i(0x03, rd, 3, rs1, imm) }
    pub fn csrrw(rd: u32, csr: u32, rs1: u32) -> u32 { i(0x73, rd, 1, rs1, csr as i32) }
    pub fn csrrs(rd: u32, csr: u32, rs1: u32) -> u32 { i(0x73, rd, 2, rs1, csr as i32) }

    pub fn bytes(code: &[u32]) -> Vec<u8> {
        code.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// Prints a string through the NS16550 UART, then powers off via the test finisher.
    pub fn hello() -> Vec<u8> {
        let msg = b"Hello from ruvm (RISC-V 64)!\n\0";
        let c = [
            lui(5, 0x10000),          //  0: t0 = 0x1000_0000 (UART)
            auipc(6, 0),              //  1: t1 = pc
            addi(6, 6, 12 * 4 - 4),   //  2: t1 -> message
            lbu(7, 6, 0),             //  3: loop: t2 = *t1
            beq(7, 0, 16),            //  4: if t2 == 0 goto done
            sb(5, 7, 0),              //  5: *uart = t2
            addi(6, 6, 1),            //  6: t1++
            jal(0, -16),              //  7: goto loop
            lui(5, 0x100),            //  8: done: t0 = 0x10_0000 (test finisher)
            lui(7, 5),                //  9
            addi(7, 7, 0x555),        // 10: t2 = 0x5555 (PASS)
            sw(5, 7, 0),              // 11: power off
        ];
        let mut v = bytes(&c);
        v.extend_from_slice(msg);
        v
    }
}

/// AArch64 instruction encoders.
pub mod a64 {
    pub fn movz(rd: u32, imm16: u32, hw: u32) -> u32 { 0xd280_0000 | hw << 21 | imm16 << 5 | rd }
    pub fn movk(rd: u32, imm16: u32, hw: u32) -> u32 { 0xf280_0000 | hw << 21 | imm16 << 5 | rd }
    pub fn adr(rd: u32, byte_off: i32) -> u32 {
        let u = byte_off as u32 & 0x1f_ffff;
        0x1000_0000 | (u & 3) << 29 | (u >> 2) << 5 | rd
    }
    pub fn add_reg(rd: u32, rn: u32, rm: u32) -> u32 { 0x8b00_0000 | rm << 16 | rn << 5 | rd }
    pub fn subs_imm(rd: u32, rn: u32, imm12: u32) -> u32 { 0xf100_0000 | imm12 << 10 | rn << 5 | rd }
    pub fn ldrb_post(rt: u32, rn: u32, imm9: i32) -> u32 {
        0x3840_0400 | ((imm9 as u32) & 0x1ff) << 12 | rn << 5 | rt
    }
    pub fn strb(rt: u32, rn: u32) -> u32 { 0x3900_0000 | rn << 5 | rt }
    pub fn cbz_w(rt: u32, off_words: i32) -> u32 { 0x3400_0000 | ((off_words as u32) & 0x7ffff) << 5 | rt }
    pub fn b(off_words: i32) -> u32 { 0x1400_0000 | (off_words as u32 & 0x03ff_ffff) }
    pub fn b_cond(cond: u32, off_words: i32) -> u32 { 0x5400_0000 | ((off_words as u32) & 0x7ffff) << 5 | cond }
    pub fn svc(imm: u32) -> u32 { 0xd400_0001 | imm << 5 }
    pub fn hvc(imm: u32) -> u32 { 0xd400_0002 | imm << 5 }

    pub fn bytes(code: &[u32]) -> Vec<u8> {
        code.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// Prints a string through the PL011 UART, then powers off via PSCI (HVC).
    pub fn hello() -> Vec<u8> {
        let msg = b"Hello from ruvm (AArch64)!\n\0";
        let c = [
            movz(1, 0x900, 1),         //  0: x1 = 0x0900_0000 (PL011)
            adr(2, (10 - 1) * 4),      //  1: x2 -> message
            ldrb_post(3, 2, 1),        //  2: loop: w3 = *x2++
            cbz_w(3, 3),               //  3: if w3 == 0 goto done
            strb(3, 1),                //  4: *uart = w3
            b(-3),                     //  5: goto loop
            movz(0, 0x0008, 0),        //  6: done: x0 = 0x8400_0008 (PSCI SYSTEM_OFF)
            movk(0, 0x8400, 1),        //  7
            hvc(0),                    //  8
            b(0),                      //  9: spin
        ];
        let mut v = bytes(&c);
        v.extend_from_slice(msg);
        v
    }
}
