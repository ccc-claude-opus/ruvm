# ruvm

A small QEMU-like machine emulator in Rust (no dependencies). It interprets
**RISC-V 64** and **AArch64** guests on a QEMU-`virt`-style board.

v0.2 adds the system-level pieces needed for kernels: RISC-V M/S/U modes with
trap delegation, Sv39 paging, the C extension, CLINT timer/software interrupts and
a PLIC; AArch64 EL0/EL1 exceptions, exclusives and LSE atomics. Both are checked
instruction-for-instruction against `qemu-system-*` (see [doc/v0.2.md](doc/v0.2.md)).

**v0.11: ISA completion** — AArch64 FP estimates / CRC32 / AES / SHA / CASP; RISC-V Zba/Zbb/Zbc/Zbs, Zicond, Zicbom/Zicboz,
Svinval, Sv48 and enforced PMP; all checked against QEMU ([doc/v0.11.md](doc/v0.11.md)).

**v0.10: SMP** — `-smp N` (1–8 CPUs) on both architectures: round-robin scheduling, SBI HSM/IPI/RFENCE, multi-CPU GICv2,
PSCI `CPU_ON`; Linux and xv6 run on several CPUs ([doc/v0.10.md](doc/v0.10.md)).

**v0.9: networking** — `-nic user[,hostfwd=tcp::H-:G]`: virtio-net + a user-mode stack (DHCP, DNS, TCP/UDP to the
host and the internet, port forwarding), no root needed ([doc/v0.9.md](doc/v0.9.md)).

**v0.8** is a speed pass with honest numbers (~120 MIPS; [doc/v0.8.md](doc/v0.8.md), `tools/bench.sh`).

**v0.7: a usable system** — ext4 root on virtio-blk, virtio-rng, RTC, real reboot; both architectures
boot from a disk image, write to it and keep the data across a reboot ([doc/v0.7.md](doc/v0.7.md)):

```
tools/linux/fetch-debian.sh && tools/linux/mkroot.sh
./target/release/ruvm -M riscv64 -m 256 -kernel tests/guest/out/linux/debian/riscv64/linux \
    -initrd tests/guest/out/linux/root/riscv64/initramfs.cpio.gz \
    -drive file=tests/guest/out/linux/root/riscv64/root.ext4 -rng \
    -append "console=ttyS0 rdinit=/init" -serial stdio
```

**v0.6 boots Linux on AArch64 too**: Debian 13's stock arm64 kernel (6.12) reaches a busybox
shell and powers off through PSCI — MMU, GICv2, generic timer, PL011, FP and Advanced SIMD, all checked against
`qemu-system-aarch64` ([doc/v0.6.md](doc/v0.6.md)):

```
tools/linux/fetch-debian.sh arm64
./target/release/ruvm -M aarch64 -m 1024 -kernel tests/guest/out/linux/debian/arm64/linux \
    -initrd tests/guest/out/linux/debian/arm64/initrd.gz -append "console=ttyAMA0 rdinit=/bin/sh" -serial stdio
```

**v0.5 boots Linux on RISC-V**: Debian 13's stock riscv64 kernel (6.12) runs to a
busybox shell and powers off cleanly ([doc/v0.5.md](doc/v0.5.md)):

```
tools/linux/fetch-debian.sh riscv64
./target/release/ruvm -M riscv64 -m 512 -kernel tests/guest/out/linux/debian/riscv64/linux \
    -initrd tests/guest/out/linux/debian/riscv64/initrd.gz -append "console=ttyS0 rdinit=/bin/sh" -serial stdio
```

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
| Interrupts | CLINT @ `0x0200_0000`, PLIC @ `0x0c00_0000` | GICv2 (PL011 = SPI 1, timer PPIs 27/30) |
| Interrupt controller | CLINT + PLIC | GICv2 @ `0x0800_0000`, generic timer |
| Disk | virtio-mmio block @ `0x1000_1000` (PLIC irq 1, with `-drive`) | virtio-mmio block @ `0x0a00_0000` (SPI 16) |
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
src/virtio/        virtio-mmio (v2) transport; blk / rng / net back-ends
src/net/          user-mode network stack for virtio-net
src/cpu/aarch64.rs  A64 core: integer, EL0/EL1 exceptions, exclusives, LSE, generic timer, IRQs
src/cpu/aarch64/    mmu.rs (stage-1 MMU), fp.rs (scalar FP), neon.rs + simd.rs (Advanced SIMD)
src/gic.rs        ARM GICv2
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

* AArch64 guests whose output equals `qemu-system-aarch64 -M virt -cpu max`: `a64_fp` (182 scalar FP
  groups x all rounding modes), `a64_neon` (~2000 Advanced SIMD groups, generated by `gen_a64_neon.py`),
  `a64_mmu` (stage-1 faults, blocks, TTBR1, TBI, LDTR/STTR, DC ZVA, EL0), `a64_irq` (GICv2, timer, PL011 IRQ);
* `rv_fp`: every F/D instruction over special operands in all rounding modes, hashed
  results + flags equal to qemu-system-riscv64;
* `rv_sbi`: an S-mode payload printing through SBI gives the same output with ruvm's
  built-in SBI, with OpenSBI running on ruvm, and with OpenSBI on QEMU;
* (ignored by default, `cargo test --release -- --ignored`) Debian's riscv64 and arm64 Linux boot to a
  shell, run commands and power off;
* xv6-riscv boots from the virtio disk and runs shell commands typed with `-input`
  (`tests/guest/build_xv6.sh` rebuilds the pinned kernel and file system).

`tests/guest/build.sh` rebuilds the user-level programs (Linux cross gcc, qemu-user);
`tests/guest/build_sys.sh` rebuilds the system tests (`riscv64-unknown-elf-gcc`, clang/lld,
qemu-system) and refreshes their reference output.

## Not implemented (yet)

Both: multi-core, network devices, a faster execution engine (JIT / block cache).
RISC-V: PMP enforcement, Sv48/57, V (vector). AArch64: EL2/EL3, SVE, FP estimate instructions
(`FRECPE`...), CRC32/crypto, 16K/64K granules, hardware A/D updates.
