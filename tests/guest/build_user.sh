#!/bin/sh
# Build the user-mode (-user) test programs: freestanding static ELFs for both architectures.
# Needs: clang + ld.lld.
set -e
cd "$(dirname "$0")"
mkdir -p out prebuilt
F="-O2 -ffreestanding -fno-builtin -fno-stack-protector -nostdlib -static -fuse-ld=lld -Wl,--build-id=none"
clang --target=riscv64-unknown-elf -march=rv64imafdc -mabi=lp64d -mno-relax $F -o out/user_rv.elf user_hello.c
clang --target=aarch64-none-elf $F -o out/user_a64.elf user_hello.c
cp out/user_rv.elf out/user_a64.elf prebuilt/
