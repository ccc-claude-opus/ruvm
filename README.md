# ruvm

A small QEMU-like machine emulator in Rust (no dependencies). It interprets
**RISC-V 64** and **AArch64** guests on a QEMU-`virt`-style board.

```
cargo build --release
./target/release/ruvm -M riscv64 -demo hello
./target/release/ruvm -M aarch64 -demo hello
./target/release/ruvm -M riscv64 -kernel prog.elf [-m 256] [-d] [-regs] [-icount N] [-serial stdio]
```

| | riscv64 | aarch64 |
|---|---|---|
| RAM | `0x8000_0000` | `0x4000_0000` |
| UART | NS16550 @ `0x1000_0000` | PL011 @ `0x0900_0000` |
| Power-off | write `0x5555` to `0x10_0000` (SiFive test finisher) | PSCI `SYSTEM_OFF` via `HVC`/`SMC` |
| Host syscalls | `ecall`, a7=93 exit / 64 write (while `mtvec==0`) | `svc #0`, x8=93 exit / 64 write |

Images: ELF64 (little-endian, `PT_LOAD` segments, entry = start PC) or raw binary
(loaded at the start of RAM). Exit status of `ruvm` is the guest's exit code.

## Layout

```
src/bus.rs        physical address space: RAM + MMIO devices
src/devices.rs    UART (16550 / PL011), test finisher
src/cpu/riscv.rs  RV64IMA + Zicsr, M-mode traps
src/cpu/aarch64.rs  A64 integer subset
src/host.rs       Linux-style exit/write "semihosting"
src/machine.rs    board wiring, ELF/raw loader, run loop
src/demo.rs       tiny encoders + built-in hello programs
```

Adding an architecture = implement `cpu::Cpu` (`step`, `pc`, `set_pc`, `dump`) and
add a memory map in `Machine::new`.

## Testing

`cargo test` runs hand-assembled programs plus prebuilt guest ELFs:

* a gcc-compiled C program (`-O2`, RISC-V and AArch64) whose output must equal the native x86 build;
* two assembly programs executing ~300 instruction variants over 361 operand pairs,
  whose raw result/NZCV dumps must hash-match `qemu-riscv64` / `qemu-aarch64`.

`tests/guest/build.sh` rebuilds all of it (needs the cross gcc toolchains; refreshes
reference hashes if qemu-user is installed).

## Not implemented (yet)

Both: MMU / paging, interrupts and timers, device tree, SIMD/FP, multi-core, block/net devices, JIT.
RISC-V: C (compressed), F/D, S/U modes. AArch64: exception levels & vectors, exclusives/atomics
(`LDXR`/`STXR`/LSE), FP/NEON; `MRS`/`MSR` only has `NZCV`, `CurrentEL` and a plain register file.
