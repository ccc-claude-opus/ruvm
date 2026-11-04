typedef unsigned long u64;
typedef unsigned int u32;
#define UART ((volatile u32 *)0x09000000)
static void putc(char c) { *UART = c; }
static void puts(const char *s) { while (*s) putc(*s++); }
static void puthex(u64 v) { for (int i = 60; i >= 0; i -= 4) putc("0123456789abcdef"[(v >> i) & 15]); }

static u64 h;
static int dbg;
static void mix(u64 v) { if (dbg) { puthex(v); putc('\n'); } for (int i = 0; i < 8; i++) { h ^= (v >> (8 * i)) & 0xff; h *= 0x100000001b3UL; } }
static u64 getfpsr(void) { u64 f; asm volatile("mrs %0, fpsr" : "=r"(f)); asm volatile("msr fpsr, xzr"); return f; }
static void setrm(u64 rm) { asm volatile("msr fpcr, %0" ::"r"(rm << 22)); }
static u64 getnzcv(void) { return 0; }

static const u64 D[] = {
    0x0000000000000000, 0x8000000000000000, 0x3ff0000000000000, 0xbff0000000000000, 0x4008000000000000,
    0x3fb999999999999a, 0x7fefffffffffffff, 0x0010000000000000, 0x000fffffffffffff, 0x0000000000000001,
    0x7ff0000000000000, 0xfff0000000000000, 0x7ff8000000000000, 0x7ff4000000000000, 0x4330000000000001,
    0xc3e0000000000000, 0x43e0000000000000, 0x41dfffffffc00000, 0x3fe0000000000000, 0xc00921fb54442d18,
    0x3ca0000000000000, 0x47efffffe0000000, 0x380fffffe0000000, 0x36a0000000000000, 0x7ff8dead00000001,
    0xfff4000000000123, 0x3ff8000000000000, 0xbfe0000000000000, 0x4004000000000000,
};
static const u32 S[] = {
    0x00000000, 0x80000000, 0x3f800000, 0xbf800000, 0x40400000, 0x3dcccccd, 0x7f7fffff, 0x00800000,
    0x007fffff, 0x00000001, 0x7f800000, 0xff800000, 0x7fc00000, 0x7fa00000, 0x4b000001, 0xdf000000,
    0x5f000000, 0x4effffff, 0x3f000000, 0xc0490fdb, 0x33800000, 0x34000001, 0x7fc01234, 0xffa00045,
    0x3fc00000, 0xbf000000, 0x40200000,
};
static const u32 H[] = {
    0x0000, 0x8000, 0x3c00, 0xbc00, 0x7bff, 0x0400, 0x03ff, 0x0001, 0x7c00, 0xfc00, 0x7e00, 0x7d00, 0x3555, 0x5640,
    0x7d55, 0xfe01,
};
#define ND (sizeof D / sizeof D[0])
#define NS (sizeof S / sizeof S[0])
#define NH (sizeof H / sizeof H[0])
#define RMS for (int rm = 0; rm < 4; rm++) { setrm(rm);
#define ENDRMS } setrm(0);

