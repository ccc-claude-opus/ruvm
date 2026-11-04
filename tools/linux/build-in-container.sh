#!/bin/sh
# Runs inside the container: build Linux + busybox for one arch, write to /out/<arch>/.
#   build-in-container.sh riscv64|arm64
set -e
ARCH=$1
KVER=6.12
BBVER=1.36.1
case $ARCH in
    riscv64) KARCH=riscv; CROSS=riscv64-linux-gnu-; IMAGE=arch/riscv/boot/Image ;;
    arm64)   KARCH=arm64; CROSS=aarch64-linux-gnu-; IMAGE=arch/arm64/boot/Image ;;
    *) echo "usage: $0 riscv64|arm64"; exit 1 ;;
esac
cd /work
[ -f linux-$KVER.tar.xz ] || wget -q https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$KVER.tar.xz
[ -d linux-$KVER ] || tar xf linux-$KVER.tar.xz
[ -f busybox-$BBVER.tar.bz2 ] || wget -q https://busybox.net/downloads/busybox-$BBVER.tar.bz2
[ -d busybox-$BBVER ] || tar xf busybox-$BBVER.tar.bz2

# ---- kernel (out-of-tree build dir per arch)
K=/work/build-linux-$ARCH
mkdir -p $K /out/$ARCH
make -s -C linux-$KVER O=$K ARCH=$KARCH CROSS_COMPILE=$CROSS tinyconfig
linux-$KVER/scripts/kconfig/merge_config.sh -m -O $K $K/.config /scripts/common.config /scripts/$ARCH.config >/dev/null
make -s -C linux-$KVER O=$K ARCH=$KARCH CROSS_COMPILE=$CROSS olddefconfig
make -s -C linux-$KVER O=$K ARCH=$KARCH CROSS_COMPILE=$CROSS -j$(nproc) Image
cp $K/$IMAGE /out/$ARCH/Image
cp $K/.config /out/$ARCH/kernel.config

# ---- busybox (static)
B=/work/build-busybox-$ARCH
mkdir -p $B
make -s -C busybox-$BBVER O=$B CROSS_COMPILE=$CROSS defconfig >/dev/null
sed -i -e 's/# CONFIG_STATIC is not set/CONFIG_STATIC=y/' -e 's/CONFIG_TC=y/# CONFIG_TC is not set/' $B/.config
make -s -C $B CROSS_COMPILE=$CROSS -j$(nproc) >/dev/null 2>&1 || make -C $B CROSS_COMPILE=$CROSS
R=/work/rootfs-$ARCH
rm -rf $R && mkdir -p $R
make -s -C $B CROSS_COMPILE=$CROSS CONFIG_PREFIX=$R install >/dev/null
mkdir -p $R/proc $R/sys $R/dev $R/tmp $R/etc $R/root
cp /scripts/init $R/init && chmod +x $R/init
(cd $R && find . | cpio -o -H newc --quiet | gzip -9) > /out/$ARCH/initramfs.cpio.gz
ls -la /out/$ARCH
