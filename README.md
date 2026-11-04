# ruvm

A small QEMU-like machine emulator in Rust (no dependencies). It interprets
**RISC-V 64** and **AArch64** guests on a QEMU-`virt`-style board.

v0.2 adds the system-level pieces needed for kernels: RISC-V M/S/U modes with
trap delegation, Sv39 paging, the C extension, CLINT timer/software interrupts and
a PLIC; AArch64 EL0/EL1 exceptions, exclusives and LSE atomics. Both are checked
instruction-for-instruction against `qemu-system-*` (see [doc/v0.2.md](doc/v0.2.md)).

v0.4 adds bit-exact F/D floating point, a device-tree generator, a built-in SBI and the
Linux `Image` boot protocol (`-kernel/-initrd/-append`), and runs real OpenSBI firmware
(`-bios`) ([doc/v0.4.md](doc/v0.4.md)).

v0.3 boots **MIT xv6-riscv** to its shell from a virtio block device
([doc/v0.3.md](doc/v0.3.md)). The goal is Linux on both architectures — see
[doc/roadmap.md](doc/roadmap.md).

```
./target/release/ruvm -M riscv64 -kernel tests/guest/prebuilt/xv6/kernel \
    -drive file=tests/guest/prebuilt/xv6/fs.img -snapshot -serial stdio
```

```
cargo build --release
./target/release/ruvm -M riscv64 -demo hello
./target/release/ruvm -M aarch64 -demo hello
./target/release/ruvm -M riscv64 -kernel prog.elf [-m 256] [-d] [-regs] [-icount N] [-serial stdio]
./target/release/ruvm -M riscv64 -kernel kernel -drive file=disk.img [-snapshot] [-input "ls\n"]
```

| | riscv64 | aarch64 |
|---|---|---|
| RAM | `0x8000_0000` | `0x4000_0000` |
| UART | NS16550 @ `0x1000_0000` (PLIC irq 10) | PL011 @ `0x0900_0000` |
| Interrupts | CLINT @ `0x0200_0000`, PLIC @ `0x0c00_0000` | — |
| Disk | virtio-mmio block @ `0x1000_1000` (PLIC irq 1, with `-drive`) | — |
| Power-off | write `0x5555` to `0x10_0000` (SiFive test finisher) | PSCI `SYSTEM_OFF` via `HVC`/`SMC` |
| Host syscalls | `ecall`, a7=93 exit / 64 write (M-mode, while `mtvec==0`) | `svc #0`, x8=93 exit / 64 write (while `VBAR_EL1==0`) |

`mtime` / `CNTVCT_EL0` advance one tick per executed instruction (reported as 10 MHz),
so runs are deterministic; `WFI` with nothing pending skips ahead to the next timer deadline.

Images: ELF64 (little-endian, `PT_LOAD` segments, entry = start PC) or raw binary
(loaded at the start of RAM). Exit status of `ruvm` is the guest's exit code.

## Layout

```
src/bus.rs        physical address space: RAM, CLINT, PLIC, MMIO devices + IRQ lines
src/devices.rs    UART (16550 / PL011), test finisher, CLINT, PLIC
src/cpu/riscv.rs  RV64IMAFDC + Zicsr, M/S/U, Sv39 MMU + TLB, interrupts, built-in SBI
src/cpu/softfloat.rs  bit-exact IEEE 754 binary32/64 (all rounding modes, flags)
src/fdt.rs        device-tree blob writer
src/cpu/rvc.rs    compressed-instruction expander
src/virtio.rs     virtio-mmio (v2) block device with DMA
src/cpu/aarch64.rs  A64 integer subset, EL0/EL1 exceptions, exclusives, LSE
src/host.rs       Linux-style exit/write "semihosting"
src/machine.rs    board wiring, ELF/raw loader, run loop
src/demo.rs       tiny encoders + built-in hello programs
```

Adding an architecture = implement `cpu::Cpu` (`step`, `pc`, `set_pc`, `dump`) and
add a memory map in `Machine::new`.

## Testing

`cargo test` runs hand-assembled programs plus prebuilt guest ELFs:

* a gcc-compiled C program (`-O2`, RISC-V and AArch64) whose output must equal the native x86 build;
* the same C program built for `rv64imac` (compressed instructions);
* two assembly programs executing ~300 instruction variants over 361 operand pairs,
  whose raw result/NZCV dumps must hash-match `qemu-riscv64` / `qemu-aarch64`;
* two full-system tests whose console output must equal `qemu-system-riscv64 -M virt -bios none`
  and `qemu-system-aarch64 -M virt -cpu max` byte for byte:
  `rv_priv` (M→S→U, delegation, Sv39, A/D, SUM/MXR/MPRV, timer/software/PLIC interrupts) and
  `a64_priv` (vectors, SVC/BRK/UDF/aborts from EL1 and EL0, ERET, SPSel, DAIF, LSE, CAS, LDXR/STXR).

* `rv_fp`: every F/D instruction over special operands in all rounding modes, hashed
  results + flags equal to qemu-system-riscv64;
* `rv_sbi`: an S-mode payload printing through SBI gives the same output with ruvm's
  built-in SBI, with OpenSBI running on ruvm, and with OpenSBI on QEMU;
* xv6-riscv boots from the virtio disk and runs shell commands typed with `-input`
  (`tests/guest/build_xv6.sh` rebuilds the pinned kernel and file system).

`tests/guest/build.sh` rebuilds the user-level programs (Linux cross gcc, qemu-user);
`tests/guest/build_sys.sh` rebuilds the system tests (`riscv64-unknown-elf-gcc`, clang/lld,
qemu-system) and refreshes their reference output.

## Not implemented (yet)

Both: device tree, SIMD/FP, multi-core, block/net devices (virtio), JIT.
RISC-V: PMP enforcement, Sv48/57, V (vector). AArch64: MMU, GIC + generic-timer interrupts,
EL2/EL3, FP/NEON.
