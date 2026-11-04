# 路線圖：讓 RISC-V 與 ARM 都能跑 Linux

一版一版往前推進，每一版都用 QEMU 當參考答案驗證。

```mermaid
flowchart LR
    v01["v0.1<br/>使用者層級整數指令"] --> v02["v0.2<br/>RV: M/S/U, Sv39, C, CLINT/PLIC<br/>ARM: EL0/EL1, 原子指令"]
    v02 --> v03["v0.3<br/>xv6-riscv：virtio-blk, Sstc"]
    v03 --> v04["v0.4<br/>RV: F/D, DTB, OpenSBI"]
    v04 --> v05["v0.5<br/>RISC-V Linux + busybox"]
    v05 --> v06["v0.6<br/>ARM: MMU, GIC, generic timer"]
    v06 --> v07["v0.7<br/>ARM: FP/SIMD, PSCI, DTB"]
    v07 --> v08["v0.8<br/>arm64 Linux + busybox"]
    v05 -.-> perf["效能：解碼快取 / block cache"]
    v08 -.-> perf
```

| 版本 | RISC-V | AArch64 | 驗收 |
|---|---|---|---|
| v0.1 | RV64IMA（M-mode） | A64 整數子集 | gcc 編譯的 C 程式、與 qemu-user 指令級比對 |
| v0.2 | M/S/U、委派、Sv39、C、CLINT、PLIC | EL0/EL1 例外、LDXR/STXR、LSE、CAS | 與 qemu-system 輸出逐位元組相同 |
| v0.3 | virtio-blk、Sstc、批次執行 | — | xv6 開機到 shell、usertests |
| v0.4 | F/D 浮點（含 `fcsr`、NaN-boxing）、`mstatus.FS`、device tree 產生器、可載入 OpenSBI (`-bios`) | — | riscv-tests 的 rv64uf/ud；OpenSBI 印出 banner |
| v0.5 | 其他 Linux 需要的：Svpbmt/Svnapot 位元容忍、`sstatus.SD`、initramfs 載入 | — | Linux 開機到 busybox shell |
| v0.6 | — | MMU（4K granule、48-bit VA、TTBR0/1、TCR、MAIR、A/D）、GICv2/v3、generic timer (CNTV/CNTP) 中斷 | 自寫核心測試與 QEMU 比對 |
| v0.7 | — | FP/SIMD（Linux arm64 必要）、PSCI CPU_ON 等、DTB、virtio | — |
| v0.8 | — | 啟動 arm64 Linux `Image` | busybox shell |

效能是跨版本的工作：Linux 開機需要數十億個指令，目前純直譯約 100 MIPS；
加入解碼快取（每個指令只解碼一次）預期可快 2–3 倍。
