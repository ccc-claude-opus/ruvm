# Build environment for the ruvm Linux guests (kernel + busybox initramfs).
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential bc bison flex cpio xz-utils bzip2 gzip wget ca-certificates \
        libelf-dev python3 kmod rsync \
        gcc-riscv64-linux-gnu libc6-dev-riscv64-cross \
        gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /work
