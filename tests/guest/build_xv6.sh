#!/bin/sh
# Fetch and build MIT xv6-riscv (pinned commit) and refresh prebuilt/xv6/{kernel,fs.img}.
# Needs: git, riscv64-unknown-elf-gcc, a host cc (for mkfs).
# Run it on ruvm:  ruvm -M riscv64 -kernel prebuilt/xv6/kernel -drive file=prebuilt/xv6/fs.img -snapshot -serial stdio
# On QEMU:         qemu-system-riscv64 -M virt -bios none -m 128M -nographic -kernel prebuilt/xv6/kernel \
#                    -global virtio-mmio.force-legacy=false -drive file=prebuilt/xv6/fs.img,if=none,format=raw,id=x0 \
#                    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0
set -e
cd "$(dirname "$0")"
COMMIT=e93dabeb98431235c6e1a101cb96c73c923fdbc4
mkdir -p out prebuilt/xv6
if [ ! -d out/xv6-riscv ]; then
    git clone -q https://github.com/mit-pdos/xv6-riscv.git out/xv6-riscv
fi
git -C out/xv6-riscv checkout -q $COMMIT
make -C out/xv6-riscv kernel/kernel fs.img
cp out/xv6-riscv/kernel/kernel out/xv6-riscv/fs.img out/xv6-riscv/LICENSE prebuilt/xv6/
