#!/bin/sh
# Download Debian's statically linked busybox for riscv64 and arm64: real glibc programs to
# run with `ruvm -user` (user-mode emulation).
# Output: tests/guest/out/user/<arch>/busybox
set -e
cd "$(dirname "$0")/../.."
VER=${BUSYBOX:-1.37.0-6+b9}
for a in ${@:-riscv64 arm64}; do
    d=tests/guest/out/user/$a
    mkdir -p $d
    [ -s $d/busybox ] && continue
    (cd $d && curl -fsSL -o bb.deb https://deb.debian.org/debian/pool/main/b/busybox/busybox-static_${VER}_$a.deb \
        && ar x bb.deb data.tar.xz && tar xf data.tar.xz ./usr/bin/busybox && mv usr/bin/busybox busybox \
        && rm -rf usr bb.deb data.tar.xz)
done
