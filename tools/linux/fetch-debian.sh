#!/bin/sh
# Download Debian's netboot (debian-installer) kernel + initrd for riscv64 and arm64:
# official, generic distro kernels that also run on QEMU's virt boards.
# Output: tests/guest/out/linux/debian/<arch>/{linux,initrd.gz}
set -e
cd "$(dirname "$0")/../.."
REL=${REL:-trixie}
for a in ${@:-riscv64 arm64}; do
    d=tests/guest/out/linux/debian/$a
    mkdir -p $d
    for f in linux initrd.gz; do
        [ -s $d/$f ] || curl -fsSL -o $d/$f \
            https://deb.debian.org/debian/dists/$REL/main/installer-$a/current/images/netboot/debian-installer/$a/$f
    done
    ls -la $d
done
