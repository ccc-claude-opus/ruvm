// RISC-V extension test: PMP regions (OFF/TOR/NA4/NAPOT, locked), Zicbom/Zicboz enabling,
// Sv48, Svinval. M-mode drives, S-mode performs the accesses; every trap is logged as
// (cause, tval). Output must be identical on qemu-system-riscv64 -cpu max and ruvm.
typedef unsigned long u64;
typedef unsigned int u32;
#define UART ((volatile unsigned char *)0x10000000)
#define csrr(r) ({ u64 _v; asm volatile("csrr %0, " #r : "=r"(_v)); _v; })
#define csrw(r, v) asm volatile("csrw " #r ", %0" ::"r"((u64)(v)))
#define csrs(r, v) asm volatile("csrs " #r ", %0" ::"r"((u64)(v)))
#define csrc(r, v) asm volatile("csrc " #r ", %0" ::"r"((u64)(v)))

extern void enter_s(void (*fn)(void));

static void putc(char c) { *UART = c; }
static void puts(const char *s) { while (*s) putc(*s++); }
static void puthex(u64 v) { puts("0x"); for (int i = 60; i >= 0; i -= 4) putc("0123456789abcdef"[(v >> i) & 15]); }
static void say(const char *s, u64 v) { puts(s); puthex(v); putc('\n'); }

static u64 trap_cause[64], trap_tval[64];
static int ntraps;
static int skip_ra;          // instruction faults return to the caller (ra)

u64 m_trap_c(u64 cause, u64 tval, u64 epc, u64 ra) {
    if (cause == 9) return 1;                 // ecall from S: leave S-mode
    if (ntraps < 64) { trap_cause[ntraps] = cause; trap_tval[ntraps] = tval; ntraps++; }
    if (cause == 1 || cause == 12) return ra; // fetch fault: return to the caller
    return epc + 4;                           // skip the faulting 4-byte instruction
}

static void flush_log(const char *what) {
    puts(what); putc('\n');
    for (int k = 0; k < ntraps; k++) { say("  cause = ", trap_cause[k]); say("  tval  = ", trap_tval[k]); }
    ntraps = 0;
}

#define BASE_WIN   0x80400000UL
#define RO_REGION  0x80401000UL
#define TOR2_LO    0x80402000UL
#define NA4_ADDR   TOR2_LO
#define LOCKED_RO  0x80500000UL

static u64 ld(u64 a) { u64 v = 0x77; asm volatile(".option push\n.option norvc\nld %0, 0(%1)\n.option pop" : "+r"(v) : "r"(a) : "memory"); return v; }
static void sd(u64 a, u64 v) { asm volatile(".option push\n.option norvc\nsd %0, 0(%1)\n.option pop" ::"r"(v), "r"(a) : "memory"); }
static void call(u64 a) { asm volatile("jalr %0" ::"r"(a) : "ra", "memory"); }

static u64 results[16];
static void s_tests(void) {
    results[0] = ld(BASE_WIN);                  // RW region: ok
    sd(BASE_WIN + 8, 0x2222);                   // ok
    results[1] = ld(BASE_WIN + 8);
    call(BASE_WIN);                             // no X: instruction access fault
    results[2] = ld(RO_REGION);                 // TOR read-only: ok
    sd(RO_REGION + 8, 1);                       // store access fault
    results[3] = ld(NA4_ADDR);                  // TOR RW: ok
    results[4] = ld(NA4_ADDR + 0x10);           // just outside the TOR region: load access fault
    sd(NA4_ADDR + 0x10, 1);                     // store access fault
    results[5] = ld(0x90000000UL);              // no matching entry
    results[6] = ld(LOCKED_RO);                 // locked RO region, S: load ok
    sd(LOCKED_RO, 5);                           // store access fault
    asm volatile("ecall");
}

static void s_cbo(void) {
    u64 w = BASE_WIN;
    sd(w, ~0UL); sd(w + 8, ~0UL); sd(w + 56, ~0UL); sd(w + 64, ~0UL);
    asm volatile("mv a0, %0\n cbo.zero (a0)" ::"r"(w + 20) : "a0", "memory");   // zeroes w .. w+63
    results[8] = ld(w); results[9] = ld(w + 56); results[10] = ld(w + 64);
    asm volatile("mv a0, %0\n cbo.clean (a0)\n cbo.flush (a0)\n cbo.inval (a0)" ::"r"(w) : "a0", "memory");
    asm volatile("ecall");
}

static void s_svinval(void) {
    asm volatile(".insn r 0x73, 0, 0x0b, x0, x0, x0\n .insn r 0x73, 0, 0x0c, x0, x0, x0\n .insn r 0x73, 0, 0x0c, x0, x0, x1\n .insn r 0x73, 0, 0x0b, x0, x5, x6" ::: "memory");
    asm volatile("ecall");
}

// ---- Sv48
__attribute__((aligned(4096))) static u64 l3[512], l2[512], l1[512];
static void s_sv48(void) {
    u64 satp = (9UL << 60) | ((u64)l3 >> 12);
    asm volatile("csrw satp, %0\n sfence.vma" ::"r"(satp));
    results[11] = csrr(satp) >> 60;
    results[12] = ld(BASE_WIN);                       // through the identity map
    sd(BASE_WIN + 16, 0x4242);
    results[13] = ld(BASE_WIN + 16);
    ld(0x0000800000000000UL);                         // non-canonical for Sv48: page fault
    ld(0xffff800000000000UL);                         // canonical, unmapped: page fault
    sd(BASE_WIN + 0x1000, 1);                         // read-only page: store page fault
    asm volatile("csrw satp, zero\n sfence.vma");
    results[14] = csrr(satp) >> 60;
    asm volatile("ecall");
}

int main(void) {
    puts("RISC-V extension test\n");
    // ---- PMP: 6 entries. Entries are checked in order; lower numbers win.
    csrw(pmpaddr0, (BASE_WIN >> 2) | 0x1ff);                 // NAPOT 4 KiB [BASE_WIN, +4K), RW
    csrw(pmpaddr1, RO_REGION >> 2);                          // TOR lower bound (entry OFF)
    csrw(pmpaddr2, (RO_REGION + 0x1000) >> 2);               // TOR [RO_REGION, +4K), R
    csrw(pmpaddr3, (TOR2_LO + 0x10) >> 2);                   // TOR [pmpaddr2, TOR2_LO+16) would start at RO_REGION+4K = TOR2_LO, RW
    csrw(pmpaddr4, (0x80000000UL >> 2) | 0x1ffff);           // NAPOT 1 MiB image, RWX
    csrw(pmpaddr5, (LOCKED_RO >> 2) | 0x1ff);                // NAPOT 4 KiB, locked, R only
    csrw(pmpcfg0, 0x1b | (0x00UL << 8) | (0x09UL << 16) | (0x0bUL << 24) | (0x1fUL << 32) | (0x99UL << 40));
    say("pmpcfg0 readback  = ", csrr(pmpcfg0));
    say("pmpaddr5 readback = ", csrr(pmpaddr5));
    csrw(pmpaddr5, 0x1234);                                  // locked: write ignored
    say("pmpaddr5 locked   = ", csrr(pmpaddr5));
    say("pmpcfg0 (locked)  = ", (csrr(pmpcfg0) >> 40) & 0xff);
    // M-mode: the locked region is enforced against M too
    u64 m = ld(LOCKED_RO);
    sd(LOCKED_RO, 9);
    sd(BASE_WIN, 0x5555);                                    // unlocked region: M unrestricted
    say("M load locked     = ", m);
    flush_log("M-mode accesses");
    enter_s(s_tests);
    flush_log("S-mode PMP accesses");
    for (int k = 0; k < 7; k++) say("  result = ", results[k]);

    // ---- Zicbom / Zicboz: disabled in menvcfg -> illegal in S, enabled -> work
    csrw(pmpaddr0, ~0UL >> 10);
    csrw(pmpcfg0, 0x1f);                                     // everything RWX
    csrc(menvcfg, (3UL << 4) | (1UL << 6) | (1UL << 7));
    enter_s(s_cbo);
    flush_log("S-mode cbo with menvcfg disabled");
    csrs(menvcfg, (3UL << 4) | (1UL << 6) | (1UL << 7));
    enter_s(s_cbo);
    flush_log("S-mode cbo enabled");
    for (int k = 8; k < 11; k++) say("  result = ", results[k]);

    // ---- Svinval
    enter_s(s_svinval);
    flush_log("Svinval");

    // ---- Sv48 identity map: l3[0] -> l2, l2[2] (0x80000000..) -> l1; l1[0] = 2 MiB RWX megapage
    // (image), l1[2] (the window) -> a table of 4K pages whose page 1 is read-only
    static u64 lw[512] __attribute__((aligned(4096)));
    l3[0] = ((u64)l2 >> 12) << 10 | 1;
    l2[2] = ((u64)l1 >> 12) << 10 | 1;
    l1[0] = (0x80000000UL >> 12) << 10 | 0xcf;                             // V R W X A D
    l1[(BASE_WIN >> 21) & 0x1ff] = ((u64)lw >> 12) << 10 | 1;
    for (int k = 0; k < 512; k++) lw[k] = ((BASE_WIN + k * 4096UL) >> 12) << 10 | (k == 1 ? 0xc3 : 0xcf);   // page 1: no W
    enter_s(s_sv48);
    flush_log("Sv48");
    for (int k = 11; k < 15; k++) say("  result = ", results[k]);
    puts("done\n");
    return 0;
}
