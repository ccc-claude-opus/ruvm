#!/bin/sh
# Build the system-level guest tests (privilege modes, MMU, interrupts) and record
# the reference output from full-system QEMU.
# Needs: riscv64-unknown-elf-gcc, clang + ld.lld (aarch64), qemu-system-riscv64/aarch64.
set -e
cd "$(dirname "$0")"
mkdir -p out prebuilt
sed 's/@BASE@/0x80000000/' link.ld.in > out/rv.ld
sed 's/@BASE@/0x40000000/' link.ld.in > out/a64.ld
LD="-Wl,--build-id=none -Wl,--no-warn-rwx-segments"
RV="riscv64-unknown-elf-gcc -O2 -march=rv64imac_zicsr_zifencei -mabi=lp64 -mcmodel=medany -mno-relax
    -ffreestanding -fno-builtin -fno-stack-protector -fno-pie -static -nostdlib $LD"
A64="clang --target=aarch64-none-elf -march=armv8.1-a -mgeneral-regs-only -O2
     -ffreestanding -fno-builtin -nostdlib -fuse-ld=lld -Wl,--build-id=none"

$RV -T rv_priv.ld -o out/rv_priv.elf rv_priv_start.S rv_priv.c
$RV -T out/rv.ld  -o out/selftest_rvc.elf rv_start.S selftest.c      # C extension build
$A64 -T out/a64.ld -o out/a64_priv.elf a64_priv_start.S a64_priv.c
cp out/rv_priv.elf out/selftest_rvc.elf out/a64_priv.elf prebuilt/

if command -v qemu-system-riscv64 >/dev/null && command -v qemu-system-aarch64 >/dev/null; then
    timeout 20 qemu-system-riscv64 -M virt -bios none -nographic -kernel out/rv_priv.elf > prebuilt/rv_priv.expected
    timeout 20 qemu-system-aarch64 -M virt -cpu max -nographic -kernel out/a64_priv.elf > prebuilt/a64_priv.expected
    echo "reference output refreshed from qemu-system"
fi
