#!/bin/sh
# Tests -accel kvm on a host without KVM (e.g. a Mac): Docker runs qemu-system-aarch64 with
# EL2 (virtualization=on, emulated by TCG); Debian's arm64 kernel boots there at EL2 and provides
# /dev/kvm, and ruvm -accel kvm boots the test Linux (tools/linux/mkroot.sh) inside it: the ext4
# root, a reboot (a second VM) and power-off, on 2 vCPUs. Everything is emulated, so it is slow
# (minutes); on a real AArch64 Linux host just run the ruvm commands below directly.
#   tools/linux/kvm-test.sh            (needs docker, cargo-zigbuild and zig)
set -eu
cd "$(dirname "$0")/../.."
for f in tests/guest/out/linux/debian/arm64/linux tests/guest/out/linux/root/arm64/root.ext4; do
    [ -s "$f" ] || { echo "run tools/linux/fetch-debian.sh arm64 && tools/linux/mkroot.sh arm64 first"; exit 1; }
done
cargo zigbuild --release --target aarch64-unknown-linux-gnu
docker build -q -t ruvm-kvm-test - <<'EOF'
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends qemu-system-arm cpio && rm -rf /var/lib/apt/lists/*
EOF
docker run --rm -v "$PWD":/ruvm:ro ruvm-kvm-test sh -eu -c '
W=/tmp/l1; mkdir -p $W/l2; cd $W
# the L1 initramfs: ruvm test busybox, ruvm with its shared libraries, the L2 images
gunzip -c /ruvm/tests/guest/out/linux/root/arm64/initramfs.cpio.gz | cpio -id --quiet
cp /ruvm/target/aarch64-unknown-linux-gnu/release/ruvm bin/ruvm
for l in $(ldd bin/ruvm | grep -o "/[^ ]*"); do mkdir -p ".$(dirname $l)"; cp -L "$l" ".$l"; done
cp /ruvm/tests/guest/out/linux/debian/arm64/linux /ruvm/tests/guest/out/linux/root/arm64/initramfs.cpio.gz /ruvm/tests/guest/out/linux/root/arm64/root.ext4 l2/
cat > init <<"INIT"
#!/bin/busybox sh
/bin/busybox --install -s /bin
export PATH=/bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
echo "L1: Linux $(uname -r) at EL2, $(grep -c ^processor /proc/cpuinfo) CPUs"
dmesg | grep -E "kvm \[1\]: (Hyp|VHE|nVHE)" | head -2
ls -l /dev/kvm
/bin/ruvm -M aarch64 -m 256 -smp 2 -accel kvm -kernel /l2/linux -initrd /l2/initramfs.cpio.gz \
    -drive file=/l2/root.ext4 -rng -append "console=ttyAMA0 rdinit=/init ruvm.test=reboot"
echo "L2: ruvm exit status $?"
poweroff -f
INIT
chmod +x init
find . | cpio -o -H newc --quiet | gzip -1 > /tmp/l1.cpio.gz
qemu-system-aarch64 -M virt,virtualization=on -cpu max -smp 2 -m 1536 -nographic -no-reboot -nic none \
    -kernel /ruvm/tests/guest/out/linux/debian/arm64/linux -initrd /tmp/l1.cpio.gz \
    -append "console=ttyAMA0 rdinit=/init quiet"
'
