//! ruvm — a tiny QEMU-like machine emulator.
//!
//! Layout:
//!   bus      physical address space (RAM + MMIO devices)
//!   fdt      device-tree blob writer
//!   devices  UART (16550 / PL011), SiFive test finisher, CLINT, PLIC
//!   cpu      instruction-set interpreters (riscv64, aarch64)
//!   psci     PSCI firmware calls (CPU power, system off / reset)
//!   accel    hardware virtualization: Hypervisor.framework (macOS) and KVM (Linux), AArch64 hosts
//!   host     "semihosting" syscalls (exit / write)
//!   linux    user-mode emulation: run a static Linux ELF with host-served syscalls
//!   virtio   virtio-mmio transport + block / entropy / network back-ends
//!   net      user-mode network stack (ARP, DHCP, ICMP, UDP, TCP proxy, port forwarding)
//!   machine  glues CPU + bus together, loads ELF / raw images
//!   demo     tiny built-in assemblers + demo programs
pub mod accel;
pub mod bus;
pub mod cpu;
pub mod demo;
pub mod devices;
pub mod fdt;
pub mod gic;
pub mod host;
pub mod linux;
pub mod machine;
pub mod net;
pub mod psci;
pub mod virtio;

pub use machine::{is_linux_image, Arch, LinuxBoot, Machine, Stop};
