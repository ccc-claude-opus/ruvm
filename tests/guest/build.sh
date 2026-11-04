#!/bin/sh
# Rebuild the guest test programs and (if qemu-user is installed) cross-check
# ruvm against it.  Needs: gcc, riscv64-linux-gnu-gcc, aarch64-linux-gnu-gcc.
#   ./build.sh            build into out/ and refresh prebuilt/
#   ./build.sh -O1        build selftest at another optimisation level (no refresh)
set -e
cd "$(dirname "$0")"
OPT=${1:--O2}
mkdir -p out prebuilt
sed 's/@BASE@/0x80000000/' link.ld.in > out/rv.ld
sed 's/@BASE@/0x40000000/' link.ld.in > out/a64.ld
LD="-Wl,--build-id=none -Wl,-z,max-page-size=0x1000 -Wl,--no-warn-rwx-segments"
C="$OPT -ffreestanding -fno-builtin -fno-stack-protector -fno-pie -static -no-pie -nostdlib -fno-asynchronous-unwind-tables $LD"
RV="-march=rv64ima -mabi=lp64 -mcmodel=medany -mno-relax"
A64="-mgeneral-regs-only"

gcc $OPT -fno-builtin -o out/host selftest.c host_main.c
riscv64-linux-gnu-gcc  $C $RV  -T out/rv.ld  -o out/rv.elf  rv_start.S  selftest.c
aarch64-linux-gnu-gcc  $C $A64 -T out/a64.ld -o out/a64.elf a64_start.S selftest.c
riscv64-linux-gnu-gcc  -nostdlib -static -no-pie $RV  $LD -T out/rv.ld  -o out/rv_alu.elf  rv_alu.S
aarch64-linux-gnu-gcc  -nostdlib -static -no-pie      $LD -T out/a64.ld -o out/a64_alu.elf a64_alu.S

[ "$OPT" = "-O2" ] || exit 0
./out/host > prebuilt/selftest.expected
cp out/rv.elf prebuilt/selftest_rv.elf
cp out/a64.elf prebuilt/selftest_a64.elf
cp out/rv_alu.elf out/a64_alu.elf prebuilt/
if command -v qemu-riscv64 >/dev/null && command -v qemu-aarch64 >/dev/null; then
    qemu-riscv64 out/rv_alu.elf   > out/rv_alu.ref
    qemu-aarch64 out/a64_alu.elf  > out/a64_alu.ref
    python3 - <<'PY'
def fnv(d):
    h = 0xcbf29ce484222325
    for b in d:
        h = ((h ^ b) * 0x100000001b3) & 0xffffffffffffffff
    return h
for n in ("rv_alu", "a64_alu"):
    d = open(f"out/{n}.ref", "rb").read()
    open(f"prebuilt/{n}.expected", "w").write(f"{len(d)} {fnv(d):016x}\n")
PY
    echo "reference hashes refreshed from qemu-user"
fi
