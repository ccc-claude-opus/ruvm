#!/bin/sh
# Full verification: build (warnings are errors), unit + differential tests, then the Linux boot tests.
# Needs network on first run (Debian kernels). qemu-system-* is only needed to regenerate references
# (tests/guest/build_sys.sh); the checked-in references are used otherwise.
set -eu
cd "$(dirname "$0")/.."

echo "== build"
RUSTFLAGS="-D warnings" cargo build --release

echo "== tests (unit, differential vs QEMU references, xv6)"
cargo test --release

echo "== Linux images"
for a in riscv64 arm64; do
    [ -s "tests/guest/out/linux/debian/$a/linux" ] || tools/linux/fetch-debian.sh "$a"
    [ -s "tests/guest/out/linux/root/$a/root.ext4" ] || tools/linux/mkroot.sh "$a"
done

tools/linux/fetch-busybox.sh

echo "== Linux tests (boot, ext4 reboot, SMP, network; both architectures)"
cargo test --release -- --ignored --test-threads=2   # also user-mode busybox

echo "ALL OK"
