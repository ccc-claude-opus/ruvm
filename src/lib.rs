//! ruvm — a tiny QEMU-like machine emulator.
//!
//! Layout:
//!   bus      physical address space (RAM + MMIO devices)
//!   devices  UART (16550 / PL011), SiFive test finisher, CLINT, PLIC
//!   cpu      instruction-set interpreters (riscv64, aarch64)
//!   host     "semihosting" syscalls (exit / write)
//!   machine  glues CPU + bus together, loads ELF / raw images
//!   demo     tiny built-in assemblers + demo programs
pub mod bus;
pub mod cpu;
pub mod demo;
pub mod devices;
pub mod host;
pub mod machine;

pub use machine::{Arch, Machine, Stop};
