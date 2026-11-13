pub mod aarch64;
pub mod riscv;
pub mod rvc;
pub mod softfloat;

use crate::bus::Bus;
use crate::machine::Stop;

/// Interface every emulated CPU implements.
pub trait Cpu: std::any::Any {
    /// For board code that needs the concrete CPU type (boot set-up).
    fn as_any(&mut self) -> &mut dyn std::any::Any;
    fn name(&self) -> &'static str;
    fn pc(&self) -> u64;
    fn set_pc(&mut self, pc: u64);
    /// Execute exactly one instruction.
    fn step(&mut self, bus: &mut Bus) -> Result<(), Stop>;
    /// Human-readable register dump.
    fn dump(&self) -> String;
    /// When an idle (WFI) CPU would wake up by itself: its next timer deadline (board time).
    fn wake_deadline(&self, _bus: &Bus) -> u64 {
        u64::MAX
    }
    /// Execute up to `n` instructions, advancing the board clock one tick per
    /// instruction. Returns how many ran and why it stopped early, if it did.
    fn run(&mut self, bus: &mut Bus, n: u64) -> (u64, Option<Stop>) {
        for k in 0..n {
            bus.clint.mtime += 1;
            if let Err(s) = self.step(bus) {
                return (k + 1, Some(s));
            }
            if let Some(code) = bus.halt {
                return (k + 1, Some(Stop::Halt(code)));
            }
        }
        (n, None)
    }
}

/// Sign-extend the low `bits` bits of `v` to 64 bits.
#[inline]
pub fn sext(v: u64, bits: u32) -> u64 {
    let s = 64 - bits;
    (((v << s) as i64) >> s) as u64
}
