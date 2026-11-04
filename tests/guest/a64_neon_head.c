typedef unsigned long u64;
typedef unsigned int u32;
#define UART ((volatile u32 *)0x09000000)
static void putc(char c) { *UART = c; }
static void puts(const char *s) { while (*s) putc(*s++); }
static void puthex(u64 v) { for (int i = 60; i >= 0; i -= 4) putc("0123456789abcdef"[(v >> i) & 15]); }

static u64 h;
static int dbg;
static void mix(u64 v) { for (int i = 0; i < 8; i++) { h ^= (v >> (8 * i)) & 0xff; h *= 0x100000001b3UL; } }
static u64 getfpsr(void) { u64 f; asm volatile("mrs %0, fpsr" : "=r"(f)); asm volatile("msr fpsr, xzr"); return f; }
static void setrm(u64 rm) { asm volatile("msr fpcr, %0" ::"r"(rm << 22)); }
static void rec(unsigned i, unsigned j, u64 a, u64 b) {
    u64 f = getfpsr();
    if (dbg) { puthex(i); putc(' '); puthex(j); putc(' '); puthex(a); putc(' '); puthex(b); putc(' '); puthex(f); putc('\n'); }
    mix(a); mix(b); mix(f);
}
#define RMS for (int rm = 0; rm < 4; rm++) { setrm(rm);
#define ENDRMS } setrm(0);

// integer edge patterns (two 64-bit halves each)
static const u64 PI[][2] __attribute__((aligned(16))) = {
    {0x0000000000000000, 0x0000000000000000}, {0xffffffffffffffff, 0xffffffffffffffff},
    {0x8000000000000000, 0x8000000000000000}, {0x7fffffffffffffff, 0x7fffffffffffffff},
    {0x0123456789abcdef, 0xfedcba9876543210}, {0x5555555555555555, 0xaaaaaaaaaaaaaaaa},
    {0x0001000200030004, 0x0005000600070008}, {0x80ff7f01807f01ff, 0x00ff00ff00ff00ff},
    {0xdeadbeefcafef00d, 0x0f1e2d3c4b5a6978}, {0x00000000ffffffff, 0xffffffff00000000},
    {0x0000000100000002, 0x7fffffff80000000}, {0xfffefffd00030002, 0x8000800100017fff},
    {0x1020304050607080, 0xf0e0d0c0b0a09080}, {0x0000000000000001, 0x8000000000000000},
    {0x3f8000003f800000, 0x3f8000003f800000}, {0x00ff00ff00ff00ff, 0xff00ff00ff00ff00},
};
// float patterns: words picked from special binary32 values
static const u32 FS[] = {
    0x00000000, 0x80000000, 0x3f800000, 0xbf800000, 0x40400000, 0x3dcccccd, 0x7f7fffff, 0x00800000,
    0x007fffff, 0x00000001, 0x7f800000, 0xff800000, 0x7fc00000, 0x7fa00000, 0x4b000001, 0xdf000000,
    0x5f000000, 0x4effffff, 0x3f000000, 0xc0490fdb, 0x33800000, 0x34000001, 0x7fc01234, 0xffa00045,
    0x3fc00000, 0xbf000000, 0x40200000, 0x4f000000, 0xcf000000, 0x3effffff,
};
static const u64 FD[] = {
    0x0000000000000000, 0x8000000000000000, 0x3ff0000000000000, 0xbff0000000000000, 0x4008000000000000,
    0x3fb999999999999a, 0x7fefffffffffffff, 0x0010000000000000, 0x000fffffffffffff, 0x0000000000000001,
    0x7ff0000000000000, 0xfff0000000000000, 0x7ff8000000000000, 0x7ff4000000000000, 0x4330000000000001,
    0xc3e0000000000000, 0x43e0000000000000, 0x41dfffffffc00000, 0x3fe0000000000000, 0xc00921fb54442d18,
    0x3ca0000000000000, 0x47efffffe0000000, 0x380fffffe0000000, 0x36a0000000000000, 0x7ff8dead00000001,
    0xfff4000000000123, 0x3ff8000000000000, 0xbfe0000000000000, 0x4004000000000000, 0x41e0000000000000,
};
#define NP 16
static u64 PF[NP][2] __attribute__((aligned(16))), PD[NP][2] __attribute__((aligned(16)));
static void init_tables(void) {
    unsigned nf = sizeof FS / 4, nd = sizeof FD / 8;
    for (unsigned k = 0; k < NP; k++) {
        PF[k][0] = (u64)FS[(k * 4) % nf] | (u64)FS[(k * 3 + 7) % nf] << 32;
        PF[k][1] = (u64)FS[(k * 5 + 2) % nf] | (u64)FS[(k * 7 + 11) % nf] << 32;
        PD[k][0] = FD[(k * 2) % nd];
        PD[k][1] = FD[(k * 5 + 3) % nd];
    }
}
