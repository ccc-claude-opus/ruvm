#!/bin/sh
# Build Linux guests for ruvm with Docker:  tools/linux/build.sh [riscv64] [arm64]
# Output: tests/guest/out/linux/<arch>/{Image,initramfs.cpio.gz,kernel.config}
set -e
cd "$(dirname "$0")"
OUT=$(cd ../.. && pwd)/tests/guest/out/linux
mkdir -p "$OUT"
docker build -q -t ruvm-linux-build . >/dev/null
for a in ${@:-riscv64 arm64}; do
    docker run --rm -v ruvm-linux-work:/work -v "$PWD":/scripts:ro -v "$OUT":/out \
        ruvm-linux-build sh /scripts/build-in-container.sh $a
done
