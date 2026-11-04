pub mod aarch64;
pub mod riscv;
pub mod rvc;

use crate::bus::Bus;
use crate::machine::Stop;

/// Interface every emulated CPU implements.
pub trait Cpu {
    fn name(&self) -> &'static str;
    fn pc(&self) -> u64;
    fn set_pc(&mut self, pc: u64);
    /// Execute exactly one instruction.
    fn step(&mut self, bus: &mut Bus) -> Result<(), Stop>;
    /// Human-readable register dump.
    fn dump(&self) -> String;
}

/// Sign-extend the low `bits` bits of `v` to 64 bits.
#[inline]
pub fn sext(v: u64, bits: u32) -> u64 {
    let s = 64 - bits;
    (((v << s) as i64) >> s) as u64
}
