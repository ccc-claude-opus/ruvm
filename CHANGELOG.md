# Changelog

Each version has its own notes in `doc/v<version>.md`.

## 1.2.0

The first release on crates.io.

- Hardware virtualization: `-accel hvf` (macOS) and `-accel kvm` (Linux) run an AArch64 guest directly on an AArch64 host, one thread per vCPU ([doc/v1.2.md](doc/v1.2.md)).
- `-drive file=…,snapshot=on`: keep one disk's writes in memory (the other drives are written as usual).

## Earlier versions

| Version | Added |
|---|---|
| 1.1 | user-mode emulation `-user`: static and dynamically linked Linux ELF programs, fork/execve/pipes, signals, threads and futexes |
| 1.0 | consolidation: CI script, cleanup, docs |
| 0.11 | ISA completion: RISC-V B/Zicond/Zicbo*/Svinval/Sv48/PMP; AArch64 FP estimates, CRC32, AES, SHA-1/SHA-256, CASP |
| 0.10 | SMP: `-smp N` (1–8 CPUs) on RISC-V and AArch64 |
| 0.9 | networking: virtio-net with a user-mode network stack |
| 0.8 | speed: class-based A64 decode, batched run loops, faster TLB path |
| 0.7 | a usable system: ext4 root on virtio-blk, virtio-rng, RTC, real reboot |
| 0.6 | boots Linux on AArch64 (Debian 13 stock arm64 kernel) |
| 0.5 | boots Linux on RISC-V (Debian 13 stock riscv64 kernel) |
| 0.4 | RISC-V F/D, device tree, built-in SBI, Linux Image boot, OpenSBI |
| 0.3 | boots xv6-riscv (virtio-mmio block device, Sstc) |
| 0.2 | RISC-V M/S/U, Sv39, C, CLINT/PLIC; AArch64 EL0/EL1 exceptions and atomics |
| 0.1 | the first version |
