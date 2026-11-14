#!/bin/sh
# Download a minimal Debian sysroot (libc6 + dash, dynamically linked glibc) for riscv64 and arm64:
# something to run with `ruvm -L <sysroot> -user <sysroot>/usr/bin/dash` (dynamic linking in user-mode emulation).
# Output: tests/guest/out/user/<arch>/sysroot/{bin,lib,usr}
set -e
cd "$(dirname "$0")/../.."
REL=${REL:-trixie}
MIRROR=https://deb.debian.org/debian
for a in ${@:-riscv64 arm64}; do
    d=tests/guest/out/user/$a
    [ -x $d/sysroot/usr/bin/dash ] && continue
    mkdir -p $d/dl $d/sysroot
    curl -fsSL $MIRROR/dists/$REL/main/binary-$a/Packages.xz | xz -dc > $d/dl/Packages
    for p in libc6 dash; do
        f=$(awk -v p="$p" 'BEGIN{RS="";FS="\n"} {n="";f="";for(i=1;i<=NF;i++){if($i=="Package: " p)n=1; if($i ~ /^Filename: /)f=substr($i,11)} if(n)print f}' $d/dl/Packages)
        [ -n "$f" ] || { echo "$p not found for $a"; exit 1; }
        curl -fsSL -o $d/dl/$p.deb $MIRROR/$f
        (cd $d/dl && ar x $p.deb && tar xf data.tar.* -C ../sysroot)
    done
    # Debian is usr-merged: /lib and /bin are symlinks into /usr
    (cd $d/sysroot && for l in lib bin sbin; do [ -e $l ] || ln -s usr/$l $l; done)
done
