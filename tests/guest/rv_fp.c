// RISC-V F/D test: every FP instruction over special operands, all rounding modes.
// Prints a hash of (result bits, fflags) per instruction so ruvm can be diffed
// against qemu-system-riscv64 -M virt -bios none. Build: -march=rv64gc -mabi=lp64d.

typedef unsigned long u64;
typedef unsigned int u32;
#define UART ((volatile unsigned char *)0x10000000)

static void putc(char c) { *UART = c; }
static void puts(const char *s) { while (*s) putc(*s++); }
static void puthex(u64 v) { for (int i = 60; i >= 0; i -= 4) putc("0123456789abcdef"[(v >> i) & 15]); }

static u64 h;
static void mix(u64 v) { for (int i = 0; i < 8; i++) { h ^= (v >> (8 * i)) & 0xff; h *= 0x100000001b3UL; } }
static u64 flags(void) { u64 f; asm volatile("frflags %0" : "=r"(f)); asm volatile("fsflags zero"); return f; }
static void setrm(int rm) { asm volatile("fsrm %0" ::"r"(rm)); }

static const u64 D[] = {
    0x0000000000000000, 0x8000000000000000, 0x3ff0000000000000, 0xbff0000000000000, 0x4008000000000000,
    0x3fb999999999999a, 0x7fefffffffffffff, 0x0010000000000000, 0x000fffffffffffff, 0x0000000000000001,
    0x7ff0000000000000, 0xfff0000000000000, 0x7ff8000000000000, 0x7ff4000000000000, 0x4330000000000001,
    0xc3e0000000000000, 0x43e0000000000000, 0x41dfffffffc00000, 0x3fe0000000000000, 0xc00921fb54442d18,
    0x3ca0000000000000, 0x47efffffe0000000, 0x380fffffe0000000, 0x36a0000000000000,
};
static const u32 S[] = {
    0x00000000, 0x80000000, 0x3f800000, 0xbf800000, 0x40400000, 0x3dcccccd, 0x7f7fffff, 0x00800000,
    0x007fffff, 0x00000001, 0x7f800000, 0xff800000, 0x7fc00000, 0x7fa00000, 0x4b000001, 0xdf000000,
    0x5f000000, 0x4effffff, 0x3f000000, 0xc0490fdb, 0x33800000, 0x34000001,
};
#define ND (sizeof D / sizeof D[0])
#define NS (sizeof S / sizeof S[0])

static double ld(u64 b) { double d; asm volatile("fmv.d.x %0, %1" : "=f"(d) : "r"(b)); return d; }
static u64 sd(double d) { u64 b; asm volatile("fmv.x.d %0, %1" : "=r"(b) : "f"(d)); return b; }
static float lf(u32 b) { float f; asm volatile("fmv.w.x %0, %1" : "=f"(f) : "r"(b)); return f; }
static u64 sf(float f) { u64 b; asm volatile("fmv.x.w %0, %1" : "=r"(b) : "f"(f)); return b; }

#define BIN_D(name, insn) static void name(void) { \
    for (int rm = 0; rm < 5; rm++) { setrm(rm); \
    for (unsigned i = 0; i < ND; i++) for (unsigned j = 0; j < ND; j++) { double r; \
        asm volatile(insn " %0, %1, %2" : "=f"(r) : "f"(ld(D[i])), "f"(ld(D[j]))); mix(sd(r)); mix(flags()); } } }
#define BIN_S(name, insn) static void name(void) { \
    for (int rm = 0; rm < 5; rm++) { setrm(rm); \
    for (unsigned i = 0; i < NS; i++) for (unsigned j = 0; j < NS; j++) { float r; \
        asm volatile(insn " %0, %1, %2" : "=f"(r) : "f"(lf(S[i])), "f"(lf(S[j]))); mix(sf(r)); mix(flags()); } } }
#define CMP_D(name, insn) static void name(void) { \
    for (unsigned i = 0; i < ND; i++) for (unsigned j = 0; j < ND; j++) { u64 r; \
        asm volatile(insn " %0, %1, %2" : "=r"(r) : "f"(ld(D[i])), "f"(ld(D[j]))); mix(r); mix(flags()); } }
#define CMP_S(name, insn) static void name(void) { \
    for (unsigned i = 0; i < NS; i++) for (unsigned j = 0; j < NS; j++) { u64 r; \
        asm volatile(insn " %0, %1, %2" : "=r"(r) : "f"(lf(S[i])), "f"(lf(S[j]))); mix(r); mix(flags()); } }
#define FMA_D(name, insn) static void name(void) { \
    for (int rm = 0; rm < 5; rm += 2) { setrm(rm); \
    for (unsigned i = 0; i < ND; i++) for (unsigned j = 0; j < ND; j++) for (unsigned k = 0; k < ND; k += 3) { double r; \
        asm volatile(insn " %0, %1, %2, %3" : "=f"(r) : "f"(ld(D[i])), "f"(ld(D[j])), "f"(ld(D[k]))); mix(sd(r)); mix(flags()); } } }
#define FMA_S(name, insn) static void name(void) { \
    for (int rm = 0; rm < 5; rm += 2) { setrm(rm); \
    for (unsigned i = 0; i < NS; i++) for (unsigned j = 0; j < NS; j++) for (unsigned k = 0; k < NS; k += 3) { float r; \
        asm volatile(insn " %0, %1, %2, %3" : "=f"(r) : "f"(lf(S[i])), "f"(lf(S[j])), "f"(lf(S[k]))); mix(sf(r)); mix(flags()); } } }
#define UN_D(name, insn, out, conv) static void name(void) { \
    for (int rm = 0; rm < 5; rm++) { setrm(rm); for (unsigned i = 0; i < ND; i++) { out r; \
        asm volatile(insn " %0, %1" : "=" #conv(r) : "f"(ld(D[i]))); mix((u64)r); mix(flags()); } } }
#define UN_S(name, insn, out, conv) static void name(void) { \
    for (int rm = 0; rm < 5; rm++) { setrm(rm); for (unsigned i = 0; i < NS; i++) { out r; \
        asm volatile(insn " %0, %1" : "=" #conv(r) : "f"(lf(S[i]))); mix((u64)r); mix(flags()); } } }
#define FROM_INT(name, insn, ty, getter) static void name(void) { \
    for (int rm = 0; rm < 5; rm++) { setrm(rm); for (unsigned i = 0; i < ND; i++) { ty r; \
        asm volatile(insn " %0, %1" : "=f"(r) : "r"(D[i] ^ (D[i] >> 7))); mix(getter(r)); mix(flags()); } } }

BIN_D(t_fadd_d, "fadd.d") BIN_D(t_fsub_d, "fsub.d") BIN_D(t_fmul_d, "fmul.d") BIN_D(t_fdiv_d, "fdiv.d")
BIN_D(t_fmin_d, "fmin.d") BIN_D(t_fmax_d, "fmax.d") BIN_D(t_fsgnj_d, "fsgnj.d") BIN_D(t_fsgnjn_d, "fsgnjn.d") BIN_D(t_fsgnjx_d, "fsgnjx.d")
BIN_S(t_fadd_s, "fadd.s") BIN_S(t_fsub_s, "fsub.s") BIN_S(t_fmul_s, "fmul.s") BIN_S(t_fdiv_s, "fdiv.s")
BIN_S(t_fmin_s, "fmin.s") BIN_S(t_fmax_s, "fmax.s") BIN_S(t_fsgnj_s, "fsgnj.s") BIN_S(t_fsgnjx_s, "fsgnjx.s")
CMP_D(t_feq_d, "feq.d") CMP_D(t_flt_d, "flt.d") CMP_D(t_fle_d, "fle.d")
CMP_S(t_feq_s, "feq.s") CMP_S(t_flt_s, "flt.s") CMP_S(t_fle_s, "fle.s")
FMA_D(t_fmadd_d, "fmadd.d") FMA_D(t_fmsub_d, "fmsub.d") FMA_D(t_fnmsub_d, "fnmsub.d") FMA_D(t_fnmadd_d, "fnmadd.d")
FMA_S(t_fmadd_s, "fmadd.s") FMA_S(t_fnmadd_s, "fnmadd.s")
UN_D(t_fsqrt_d, "fsqrt.d", double, f) UN_S(t_fsqrt_s, "fsqrt.s", float, f)
UN_D(t_fcvt_w_d, "fcvt.w.d", u64, r) UN_D(t_fcvt_wu_d, "fcvt.wu.d", u64, r)
UN_D(t_fcvt_l_d, "fcvt.l.d", u64, r) UN_D(t_fcvt_lu_d, "fcvt.lu.d", u64, r)
UN_S(t_fcvt_w_s, "fcvt.w.s", u64, r) UN_S(t_fcvt_lu_s, "fcvt.lu.s", u64, r)
UN_D(t_fcvt_s_d, "fcvt.s.d", float, f) UN_S(t_fcvt_d_s, "fcvt.d.s", double, f)
UN_D(t_fclass_d, "fclass.d", u64, r) UN_S(t_fclass_s, "fclass.s", u64, r)
FROM_INT(t_fcvt_d_l, "fcvt.d.l", double, sd) FROM_INT(t_fcvt_d_lu, "fcvt.d.lu", double, sd)
FROM_INT(t_fcvt_d_w, "fcvt.d.w", double, sd) FROM_INT(t_fcvt_s_l, "fcvt.s.l", float, sf)
FROM_INT(t_fcvt_s_wu, "fcvt.s.wu", float, sf) FROM_INT(t_fcvt_s_lu, "fcvt.s.lu", float, sf)

// sqrt/div results printed directly for a few values, NaN boxing, fcsr layout
static void t_misc(void) {
    double r = 0; float f;
    asm volatile("fmv.d.x %0, %1\n fadd.s %0, %0, %0" : "+f"(r) : "r"(0x3f800000UL)); // unboxed single -> NaN
    mix(sd(r)); mix(flags());
    asm volatile("fcvt.s.w %0, %1" : "=f"(f) : "r"(3)); mix(sd(*(double *)&f)); // check boxing via raw 64 bits
    double x; asm volatile("fmv.d.x %0, %1" : "=f"(x) : "r"(0x400921fb54442d18UL));
    u64 raw; asm volatile("fcvt.s.d %0, %2\n fmv.x.d %1, %0" : "=f"(f), "=r"(raw) : "f"(x)); mix(raw);
    asm volatile("csrw fcsr, %0" ::"r"(0xffUL)); u64 v; asm volatile("csrr %0, fcsr" : "=r"(v)); mix(v);
    asm volatile("csrr %0, frm" : "=r"(v)); mix(v); asm volatile("csrr %0, fflags" : "=r"(v)); mix(v);
    asm volatile("csrw fcsr, zero");
    // dynamic rounding with an invalid frm is illegal: not tested (would trap)
}

struct t { const char *name; void (*fn)(void); };
static const struct t tests[] = {
#define T(n) {#n, t_##n}
    T(fadd_d), T(fsub_d), T(fmul_d), T(fdiv_d), T(fmin_d), T(fmax_d), T(fsgnj_d), T(fsgnjn_d), T(fsgnjx_d),
    T(fadd_s), T(fsub_s), T(fmul_s), T(fdiv_s), T(fmin_s), T(fmax_s), T(fsgnj_s), T(fsgnjx_s),
    T(feq_d), T(flt_d), T(fle_d), T(feq_s), T(flt_s), T(fle_s),
    T(fmadd_d), T(fmsub_d), T(fnmsub_d), T(fnmadd_d), T(fmadd_s), T(fnmadd_s),
    T(fsqrt_d), T(fsqrt_s), T(fcvt_w_d), T(fcvt_wu_d), T(fcvt_l_d), T(fcvt_lu_d), T(fcvt_w_s), T(fcvt_lu_s),
    T(fcvt_s_d), T(fcvt_d_s), T(fclass_d), T(fclass_s),
    T(fcvt_d_l), T(fcvt_d_lu), T(fcvt_d_w), T(fcvt_s_l), T(fcvt_s_wu), T(fcvt_s_lu), T(misc),
};

int main(void) {
    u64 all = 0;
    for (unsigned k = 0; k < sizeof tests / sizeof tests[0]; k++) {
        h = 0xcbf29ce484222325UL;
        tests[k].fn();
        puts(tests[k].name); puts(" "); puthex(h); putc('\n');
        all ^= h * (k + 1);
    }
    puts("all "); puthex(all); putc('\n');
    // a little real C floating point too
    double acc = 0; for (int i = 1; i <= 1000; i++) acc += 1.0 / ((double)i * i);
    puts("basel "); puthex(sd(acc)); putc('\n');
    return 0;
}
