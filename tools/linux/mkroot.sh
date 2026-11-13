#!/bin/bash
# Build a small Linux system for ruvm from Debian packages (no Docker, no compiler):
#   tests/guest/out/linux/root/<arch>/initramfs.cpio.gz   loads the storage modules and mounts /dev/vda
#   tests/guest/out/linux/root/<arch>/root.ext4           ext4 root: busybox + /sbin/init
# Needs: curl, ar, tar, xz, cpio, gzip, mke2fs (e2fsprogs), and a previous
# tools/linux/fetch-debian.sh run (the netboot initrd supplies some modules and fixes the kernel version).
#   tools/linux/mkroot.sh [riscv64] [arm64]
set -e
cd "$(dirname "$0")/../.."
ROOT=$PWD
MKE2FS=${MKE2FS:-$(command -v mke2fs || echo /opt/homebrew/opt/e2fsprogs/sbin/mke2fs)}
REL=${REL:-trixie}
MIRROR=https://deb.debian.org/debian
DISK_MB=${DISK_MB:-64}

build() {
    ARCH=$1
    case $ARCH in
        riscv64) POOL=linux ;;
        arm64)   POOL=linux-signed-arm64 ;;
        *) echo "unknown arch $ARCH"; exit 1 ;;
    esac
    DEB=tests/guest/out/linux/debian/$ARCH
    OUT=tests/guest/out/linux/root/$ARCH
    W=$ROOT/tests/guest/out/linux/work/$ARCH
    rm -rf "$W"; mkdir -p "$W/rd" "$W/udeb" "$W/bb" "$OUT"
    [ -s $DEB/initrd.gz ] || { echo "run tools/linux/fetch-debian.sh $ARCH first"; exit 1; }

    # kernel version = the one the netboot initrd was built for
    (cd "$W/rd" && gunzip -c "$ROOT/$DEB/initrd.gz" | cpio -id --quiet 2>/dev/null)
    KVER=$(ls "$W/rd/usr/lib/modules" | head -1)
    echo "[$ARCH] kernel modules for $KVER"

    # package index (udebs)
    curl -fsSL -o "$W/Packages.xz" $MIRROR/dists/$REL/main/debian-installer/binary-$ARCH/Packages.xz
    xz -dc "$W/Packages.xz" > "$W/Packages"
    udeb() { grep -F "Filename: pool/main/l/$POOL/$1-modules-$KVER-di_" "$W/Packages" | sed 's/Filename: //'; }
    for p in ext4 scsi; do
        f=$(udeb $p); [ -n "$f" ] || { echo "udeb $p-modules-$KVER not found"; exit 1; }
        curl -fsSL -o "$W/udeb/$p.udeb" $MIRROR/$f
        (cd "$W/udeb" && mkdir $p && cd $p && ar x ../$p.udeb && tar xf data.tar.xz)
    done

    # busybox
    curl -fsSL -o "$W/bb/busybox.deb" $MIRROR/pool/main/b/busybox/busybox-static_${BUSYBOX:-1.37.0-6+b9}_$ARCH.deb
    (cd "$W/bb" && ar x busybox.deb && tar xf data.tar.*)
    BB=$(find "$W/bb" -name busybox -type f -perm -u+x | head -1)
    [ -n "$BB" ] || { echo "busybox binary not found in the deb"; exit 1; }

    # modules, decompressed (busybox insmod cannot read .xz)
    MODS=$W/modules; mkdir -p "$MODS"
    # The appended module signature is stripped: verifying it means SHA-256 over the whole
    # module, which costs a lot of emulated instructions (and the kernel does not enforce signatures).
    strip_sig() { python3 - "$1" <<'PY'
import sys
p = sys.argv[1]
d = open(p, "rb").read()
marker = b"~Module signature appended~\n"
if d.endswith(marker):
    sig_len = int.from_bytes(d[-len(marker) - 4:-len(marker)], "big")
    d = d[: len(d) - len(marker) - 12 - sig_len]
open(p, "wb").write(d)
PY
    }
    getmod() { f=$(find "$W/udeb" "$W/rd/usr/lib/modules" -name "$1.ko.xz" | head -1); [ -n "$f" ] || { echo "missing module $1"; exit 1; }; xz -dc "$f" > "$MODS/$1.ko"; strip_sig "$MODS/$1.ko"; }
    for m in crc16 crc32c_generic libcrc32c mbcache jbd2 ext4 virtio_mmio virtio_blk virtio-rng failover net_failover virtio_net; do getmod $m; done

    # ---- initramfs
    I=$W/initramfs; mkdir -p $I/bin $I/lib/modules $I/proc $I/sys $I/dev $I/mnt $I/tmp
    cp "$BB" $I/bin/busybox
    cp "$MODS"/*.ko $I/lib/modules/
    cat > $I/init <<'INIT'
#!/bin/busybox sh
/bin/busybox --install -s /bin
export PATH=/bin:/sbin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null
for m in crc16 crc32c_generic libcrc32c mbcache jbd2 ext4 virtio_mmio virtio_blk virtio-rng failover net_failover virtio_net; do
    insmod /lib/modules/$m.ko
done
i=0
while [ ! -b /dev/vda ] && [ $i -lt 30 ]; do sleep 1; i=$((i+1)); done
if mount -t ext4 /dev/vda /mnt; then
    umount /proc /sys 2>/dev/null
    exec switch_root /mnt /sbin/init
fi
echo "ruvm initramfs: cannot mount /dev/vda; dropping to a shell"
exec setsid cttyhack sh
INIT
    chmod +x $I/init
    chmod 755 $I/bin/busybox
    (cd $I && find . | cpio -o -H newc --quiet 2>/dev/null | gzip -9) > "$OUT/initramfs.cpio.gz"

    # ---- ext4 root
    R=$W/root; mkdir -p $R/bin $R/sbin $R/etc $R/proc $R/sys $R/dev $R/tmp $R/var $R/root $R/mnt
    cp "$BB" $R/bin/busybox
    chmod 755 $R/bin/busybox
    cat > $R/sbin/init <<'INIT'
#!/bin/busybox sh
/bin/busybox --install -s /bin
/bin/busybox --install -s /sbin
export PATH=/bin:/sbin HOME=/root
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null
mount -t tmpfs tmpfs /tmp
n=$(cat /var/boots 2>/dev/null || echo 0)
n=$((n+1))
echo $n > /var/boots
sync
echo "ruvm root: boot #$n on $(uname -m), kernel $(uname -r)"
[ -r /sys/class/rtc/rtc0/name ] && echo "rtc: $(cat /sys/class/rtc/rtc0/name), year $(date -u +%Y)"
if [ -c /dev/hwrng ]; then
    r=$(dd if=/dev/hwrng bs=8 count=1 2>/dev/null | od -An -tx1 | tr -d ' \n')
    echo "hwrng: ${#r} hex digits"
fi
case "$(cat /proc/cmdline)" in
*ruvm.test=reboot*)
    if [ $n -lt 2 ]; then
        echo "first boot: rebooting"
        sync
        reboot -f
    else
        echo "second boot: /var/boots persisted = $n"
        echo "done"
        poweroff -f
    fi ;;
*ruvm.test=net*)
    ifconfig lo 127.0.0.1 up
    ifconfig eth0 up
    udhcpc -i eth0 -n -q -s /etc/udhcpc.script
    echo "NET-IP: $(ifconfig eth0 | grep 'inet addr' | tr -s ' ')"
    ping -c 2 -W 3 10.0.2.2 > /dev/null && echo "NET-PING-OK"
    P=$(sed -n 's/.*ruvm.port=\([0-9]*\).*/\1/p' /proc/cmdline)
    U=$(sed -n 's/.*ruvm.uport=\([0-9]*\).*/\1/p' /proc/cmdline)
    echo "NET-HTTP: $(wget -q -O - http://10.0.2.2:$P/hello)"
    tftp -g -r hello -l /tmp/udp.out 10.0.2.2 $U > /dev/null 2>&1
    echo "NET-UDP: $(cat /tmp/udp.out 2>/dev/null)"
    echo "NET-FWD-READY"
    echo "NET-FWD-GOT: $(nc -l -p 7000 -w 30)"
    echo "done"
    poweroff -f ;;
*ruvm.test=poweroff*)
    echo "done"
    poweroff -f ;;
esac
exec setsid cttyhack sh
INIT
    chmod +x $R/sbin/init
    echo "ruvm" > $R/etc/hostname
    cat > $R/etc/udhcpc.script <<'UDHCPC'
#!/bin/sh
# udhcpc event script: configure the interface from the lease
case "$1" in
bound|renew)
    ifconfig $interface $ip netmask ${subnet:-255.255.255.0} up
    [ -n "$router" ] && route add default gw ${router%% *} dev $interface 2>/dev/null
    : > /etc/resolv.conf
    for d in $dns; do echo "nameserver $d" >> /etc/resolv.conf; done ;;
esac
exit 0
UDHCPC
    chmod +x $R/etc/udhcpc.script
    rm -f "$OUT/root.ext4"
    "$MKE2FS" -q -t ext4 -L ruvm-root -E root_owner=0:0 -d $R "$OUT/root.ext4" ${DISK_MB}M
    rm -rf "$W"
    ls -la "$OUT"
}

for a in ${@:-riscv64 arm64}; do build $a; done
