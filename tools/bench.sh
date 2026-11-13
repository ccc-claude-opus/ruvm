#!/bin/sh
# Rough speed numbers (million instructions per second) for the interpreter:
#   tools/bench.sh            ALU loops + the first 3 billion instructions of a Linux boot
# Needs the Linux images from tools/linux/fetch-debian.sh and tools/linux/mkroot.sh for the boot numbers.
set -e
cd "$(dirname "$0")/.."
cargo build --release -q
RUVM=./target/release/ruvm
mips() { # name instructions seconds
    awk -v n="$1" -v i="$2" -v s="$3" 'BEGIN { printf "%-34s %7.1f MIPS  (%.1f s)\n", n, i / s / 1e6, s }'
}
timeit() { /usr/bin/time -p "$@" 2>&1 >/dev/null | awk '/^user/ { print $2 }'; }
if [ -f tests/guest/out/spin_a64.elf ]; then
    mips "aarch64 ALU loop (600M insns)" 600000000 "$(timeit $RUVM -M aarch64 -kernel tests/guest/out/spin_a64.elf)"
fi
for a in riscv64:riscv64:256:ttyS0 aarch64:arm64:512:ttyAMA0; do
    IFS=: read -r arch dir mem con <<EOF2
$a
EOF2
    root=tests/guest/out/linux/root/$dir; k=tests/guest/out/linux/debian/$dir/linux
    [ -f "$k" ] && [ -f "$root/initramfs.cpio.gz" ] || continue
    mips "$arch Linux boot (3000M insns)" 3000000000 "$(timeit $RUVM -M $arch -m $mem -kernel $k -initrd $root/initramfs.cpio.gz -append "console=$con rdinit=/init" -icount 3000000000)"
done
