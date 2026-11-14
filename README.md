# ruvm

A QEMU-like machine emulator in Rust, with no dependencies. It interprets **RISC-V 64** and **AArch64** guests on
QEMU-`virt`-style boards, and boots **unmodified Debian Linux on both** — to a shell, from an ext4 disk, with
networking and up to 8 CPUs. Instruction behaviour is checked against `qemu-system-*`.

## Quick start

```
cargo build --release
tools/linux/fetch-debian.sh && tools/linux/mkroot.sh      # Debian kernels + a small ext4 root (once)

# RISC-V
./target/release/ruvm -M riscv64 -m 512 -smp 2 -kernel tests/guest/out/linux/debian/riscv64/linux \
    -initrd tests/guest/out/linux/root/riscv64/initramfs.cpio.gz \
    -drive file=tests/guest/out/linux/root/riscv64/root.ext4 -rng -nic user \
    -append "console=ttyS0 rdinit=/init" -serial stdio

# ARM
./target/release/ruvm -M aarch64 -m 1024 -smp 2 -kernel tests/guest/out/linux/debian/arm64/linux \
    -initrd tests/guest/out/linux/root/arm64/initramfs.cpio.gz \
    -drive file=tests/guest/out/linux/root/arm64/root.ext4 -rng -nic user \
    -append "console=ttyAMA0 rdinit=/init" -serial stdio
```

**Run a single program** (like `spike pk` / `qemu-user`): `ruvm -user hello.elf [args]` runs a static Linux ELF, serving its
system calls on the host (`tools/linux/fetch-busybox.sh` gives real glibc binaries to try; dynamically linked ones run with `-L <sysroot>`; [doc/v1.1.md](doc/v1.1.md)).

Other guests: `-demo hello`, bare ELF programs (`-kernel prog.elf`), MIT xv6-riscv
(`-kernel tests/guest/prebuilt/xv6/kernel -drive file=tests/guest/prebuilt/xv6/fs.img -snapshot`), OpenSBI (`-bios`).
Options: `-m MB`, `-smp N`, `-drive file=…[,…]`, `-snapshot`, `-rng`, `-rng-seed`, `-nic user[,hostfwd=tcp::H-:G]`,
`-rtc`, `-no-reboot`, `-input`, `-send-after`, `-d`, `-regs`, `-icount`. `ruvm -h` lists them all.

## What is implemented

| | RISC-V 64 | AArch64 |
|---|---|---|
| ISA | RV64IMAFDC, Zba/Zbb/Zbc/Zbs, Zicond, Zicbom/Zicboz, Zicsr, Svinval, Sstc | A64 integer, LSE atomics, scalar FP + Advanced SIMD (bit-exact), CRC32, AES, SHA-1/256, PMULL |
| Privilege / MMU | M/S/U, delegation, Sv39/Sv48, PMP | EL0/EL1, 4K granule, 48-bit VA, TTBR0/1 |
| Interrupts / timer | CLINT + PLIC, Sstc | GICv2, generic timer |
| Firmware | built-in SBI (HSM, IPI, RFENCE, PMU, reset) or OpenSBI | built-in PSCI 1.1 |
| SMP | up to 8 harts | up to 8 CPUs (`CPU_ON`) |

Devices (both boards): NS16550 / PL011 console, virtio-mmio block, rng and net (user-mode network stack: DHCP, DNS,
ICMP, UDP, TCP, port forwarding), RTC, power-off and reboot. A device tree is generated for each board.

**Not implemented**: RISC-V V and H, ARM EL2/EL3, SVE, pointer authentication, GICv3, PCI, graphics, JIT; in `-user` mode asynchronous signals (alarm, SIGCHLD).
Speed is that of a plain interpreter, about 120 MIPS (see [doc/v0.8.md](doc/v0.8.md)); a Debian boot takes about a minute.

## Testing

```
tools/ci.sh        # build with -D warnings, all tests, then the Linux boot tests on both architectures
cargo test --release
```

The differential tests run hand-written and generated guests (`tests/guest/`) and compare their output byte for byte with
references recorded from `qemu-system-*`; `tests/guest/build_sys.sh` regenerates both (needs QEMU and LLVM).
The `#[ignore]` tests boot real Linux (they need `fetch-debian.sh` and `mkroot.sh` first).

## History

| Version | Added |
|---|---|
| v1.1 | user-mode emulation `-user` ([doc/v1.1.md](doc/v1.1.md)) |
| v1.0 | consolidation: CI script, cleanup, docs ([doc/v1.0.md](doc/v1.0.md)) |
| v0.11 | ISA completion: FP estimates, CRC32, AES, SHA, CASP; RISC-V B, Zicond, Zicbo\*, Svinval, Sv48, PMP ([doc/v0.11.md](doc/v0.11.md)) |
| v0.10 | SMP ([doc/v0.10.md](doc/v0.10.md)) |
| v0.9 | virtio-net and a user-mode network stack ([doc/v0.9.md](doc/v0.9.md)) |
| v0.8 | speed pass ([doc/v0.8.md](doc/v0.8.md)) |
| v0.7 | ext4 root on virtio-blk, rng, RTC, reboot ([doc/v0.7.md](doc/v0.7.md)) |
| v0.6 | AArch64 Linux: MMU, GICv2, timer, FP/NEON, PSCI ([doc/v0.6.md](doc/v0.6.md)) |
| v0.5 | RISC-V Linux ([doc/v0.5.md](doc/v0.5.md)) |
| v0.4 | RISC-V F/D, device tree, SBI, OpenSBI ([doc/v0.4.md](doc/v0.4.md)) |
| v0.3 | xv6-riscv, virtio-blk ([doc/v0.3.md](doc/v0.3.md)) |
| v0.2 | privileged architecture for both ([doc/v0.2.md](doc/v0.2.md)) |
| v0.1 | user-level RV64IMA / A64 integer |

See [doc/roadmap.md](doc/roadmap.md) for the plan and what comes after 1.0.

## Memory map

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
