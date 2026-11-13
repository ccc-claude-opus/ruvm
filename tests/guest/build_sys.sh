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
# F/D test (lp64d) and an S-mode SBI payload in Linux Image format
$RV -march=rv64imafdc_zicsr -mabi=lp64d -T out/rv.ld -o out/rv_fp.elf rv_fp_start.S rv_fp.c
sed 's/@BASE@/0x80200000/; s/stack_top = .;/stack_top = .;\n  _end = .;/' link.ld.in > out/rv_sbi.ld
$RV -T out/rv_sbi.ld -o out/rv_sbi.elf rv_sbi.S
riscv64-unknown-elf-objcopy -O binary out/rv_sbi.elf out/rv_sbi.Image
# AArch64: FP / NEON (generated), MMU and interrupt guests
A64F="clang --target=aarch64-none-elf -march=armv8.2-a+fp16+aes+sha2+crc+lse -O1 -ffreestanding -fno-builtin -nostdlib -fuse-ld=lld -Wl,--build-id=none"
python3 gen_a64_fp.py >/dev/null && python3 gen_a64_neon.py >/dev/null
$A64F -T out/a64.ld -o out/a64_fp.elf a64_fp_start.S a64_fp.c
$A64F -T out/a64.ld -o out/a64_neon.elf a64_fp_start.S a64_neon.c
$A64 -T out/a64.ld -o out/a64_mmu.elf a64_priv_start.S a64_mmu.c
$A64 -T out/a64.ld -o out/a64_irq.elf a64_priv_start.S a64_irq.c
# RISC-V extensions: bit manipulation (generated) and PMP / Zicbo* / Svinval / Sv48
python3 gen_rv_bitmanip.py >/dev/null
RVX="riscv64-unknown-elf-gcc -O1 -mabi=lp64d -mcmodel=medany -mno-relax -ffreestanding -fno-builtin -nostdlib -fno-pie -static $LD"
$RVX -march=rv64imafdc_zicsr_zifencei_zba_zbb_zbc_zbs_zicond -T out/rv.ld -o out/rv_bitmanip.elf rv_fp_start.S rv_bitmanip.c
$RVX -march=rv64imafd_zicsr_zifencei_zicbom_zicboz -T out/rv.ld -o out/rv_ext.elf rv_ext_start.S rv_ext.c
# SMP guests (4 CPUs)
$RV -T out/rv.ld -o out/rv_smp.elf rv_smp_start.S rv_smp.c
$A64 -T out/a64.ld -o out/a64_smp.elf a64_priv_start.S a64_smp_sec.S a64_smp.c
cp out/rv_priv.elf out/selftest_rvc.elf out/a64_priv.elf out/rv_fp.elf out/rv_sbi.Image prebuilt/
cp out/a64_fp.elf out/a64_neon.elf out/a64_mmu.elf out/a64_irq.elf out/rv_smp.elf out/a64_smp.elf out/rv_bitmanip.elf out/rv_ext.elf prebuilt/
OPENSBI=$(dirname "$(command -v qemu-system-riscv64)")/../share/qemu/opensbi-riscv64-generic-fw_dynamic.bin
[ -f "$OPENSBI" ] && cp "$OPENSBI" prebuilt/opensbi-fw_dynamic.bin

if command -v qemu-system-riscv64 >/dev/null && command -v qemu-system-aarch64 >/dev/null; then
    timeout 20 qemu-system-riscv64 -M virt -bios none -nographic -kernel out/rv_priv.elf > prebuilt/rv_priv.expected
    timeout 20 qemu-system-aarch64 -M virt -cpu max -nographic -kernel out/a64_priv.elf > prebuilt/a64_priv.expected
    timeout 60 qemu-system-riscv64 -M virt -bios none -nographic -kernel out/rv_fp.elf > prebuilt/rv_fp.expected
    timeout 60 qemu-system-riscv64 -M virt -bios none -smp 4 -nographic -kernel out/rv_smp.elf > prebuilt/rv_smp.expected
    timeout 60 qemu-system-riscv64 -M virt -cpu max -bios none -nographic -kernel out/rv_bitmanip.elf > prebuilt/rv_bitmanip.expected
    timeout 60 qemu-system-riscv64 -M virt -cpu max -bios none -nographic -kernel out/rv_ext.elf > prebuilt/rv_ext.expected
    timeout 60 qemu-system-aarch64 -M virt -cpu max -smp 4 -nographic -kernel out/a64_smp.elf > prebuilt/a64_smp.expected
    for t in a64_fp a64_neon a64_mmu a64_irq; do
        timeout 120 qemu-system-aarch64 -M virt -cpu max -nographic -kernel out/$t.elf > prebuilt/$t.expected
    done
    # QEMU's default firmware is OpenSBI: keep only what the payload printed
    timeout 20 qemu-system-riscv64 -M virt -nographic -kernel out/rv_sbi.Image | sed -n '/S-mode payload/,$p' > prebuilt/rv_sbi.expected
    echo "reference output refreshed from qemu-system"
fi
