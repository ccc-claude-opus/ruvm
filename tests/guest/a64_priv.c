// AArch64 EL0/EL1 exception test: vectors, SVC/BRK/UDF/data aborts from EL1 and EL0,
// ERET, SP_EL0/SPSel, DAIF, exclusives, LSE atomics and CAS.
// The output must be identical on qemu-system-aarch64 (-M virt -cpu max) and ruvm.

typedef unsigned long u64;
typedef unsigned int u32;

#define UART ((volatile u32 *)0x09000000)
#define BAD_ADDR 0x0e000000UL // unassigned on the virt board: external abort

#define mrs(r) ({ u64 _v; asm volatile("mrs %0, " #r : "=r"(_v)); _v; })
#define msr(r, v) asm volatile("msr " #r ", %0" ::"r"((u64)(v)))

extern char vectors[], user_exit[];
extern void enter_user(u64 pc, u64 sp);

static void putc(char c) { *UART = c; }
static void puts(const char *s) { while (*s) putc(*s++); }
static void puthex(u64 v) {
    puts("0x");
    for (int i = 60; i >= 0; i -= 4) putc("0123456789abcdef"[(v >> i) & 15]);
}
static void say(const char *s, u64 v) { puts(s); puthex(v); putc('\n'); }

static const char *const vec_name[4] = {"cur-EL SP0", "cur-EL SPx", "lower-EL A64", "lower-EL A32"};

void exc_handler(u64 *f, int idx) {
    u64 esr = mrs(esr_el1), elr = mrs(elr_el1);
    u32 ec = esr >> 26;
    puts("  [");
    puts(vec_name[idx >> 2]);
    puts(idx & 3 ? " async] " : " sync] ");
    switch (ec) {
    case 0x15: // SVC
        say("SVC imm=", esr & 0xffff);
        if (idx >= 8) { // from EL0: syscalls
            switch (esr & 0xffff) {
            case 0: // exit
                msr(elr_el1, user_exit);
                msr(spsr_el1, 0x3c5); // EL1h, DAIF masked
                return;
            case 1: putc(f[0]); return;
            case 2: say("  EL0 says ", f[0]); return;
            }
        }
        return; // ELR already points after the SVC
    case 0x3c:
        say("BRK imm=", esr & 0xffff);
        break;
    case 0x00:
        say("undefined, ESR=", esr);
        break;
    case 0x24:
    case 0x25:
        say(ec == 0x24 ? "data abort (lower EL), DFSC=" : "data abort (same EL), DFSC=", esr & 0x3f);
        say("    WnR=", (esr >> 6) & 1);
        say("    FAR=", mrs(far_el1));
        break;
    default:
        say("unexpected EC=", ec);
        say("    ELR=", elr);
        asm volatile("ldr x0, =0x84000008\n hvc #0");
    }
    say("    SPSR.M=", mrs(spsr_el1) & 0xf);
    msr(elr_el1, elr + 4);
}

// ------------------------------------------------------------------ EL0 program

static u64 svc_print(u64 v) {
    register u64 x0 asm("x0") = v;
    asm volatile("svc #2" : "+r"(x0)::"memory");
    return x0;
}

__attribute__((aligned(16))) static char user_stack[4096];
static volatile u64 shared = 40;

void user_main(void) {
    register u64 x0 asm("x0") = 'U';
    asm volatile("svc #1" : "+r"(x0));
    x0 = '\n';
    asm volatile("svc #1" : "+r"(x0));
    svc_print(mrs(nzcv)); // NZCV is EL0-accessible
    u64 v;
    asm volatile("mrs %0, vbar_el1" : "=r"(v)); // EL1 register at EL0: undefined
    asm volatile("ldr %0, [%1]" : "=r"(v) : "r"(BAD_ADDR + 8)); // data abort from EL0
    asm volatile("strb wzr, [%0]" ::"r"(BAD_ADDR + 1));
    // an LDXR/STXR increment at EL0
    u64 tmp, st;
    asm volatile("1: ldxr %0, [%2]\n add %0, %0, #2\n stxr %w1, %0, [%2]\n cbnz %w1, 1b"
                 : "=&r"(tmp), "=&r"(st) : "r"(&shared) : "memory");
    svc_print(shared);
    asm volatile("svc #0");
}

// ------------------------------------------------------------------ EL1 tests

static void atomics(void) {
    static volatile u64 m64;
    static volatile u32 m32;
    u64 old, st;

    m64 = 5;
    asm volatile("ldadd %1, %0, [%2]" : "=r"(old) : "r"(10UL), "r"(&m64) : "memory");
    say("ldadd old=", old);
    say("ldadd new=", m64);
    asm volatile("ldclral %1, %0, [%2]" : "=r"(old) : "r"(4UL), "r"(&m64) : "memory");
    say("ldclral -> ", m64);
    asm volatile("ldeor %1, %0, [%2]" : "=r"(old) : "r"(0xffUL), "r"(&m64) : "memory");
    asm volatile("stset %0, [%1]" ::"r"(0x100UL), "r"(&m64) : "memory");
    say("ldeor+stset -> ", m64);
    asm volatile("swpal %1, %0, [%2]" : "=r"(old) : "r"(0x1234UL), "r"(&m64) : "memory");
    say("swp old=", old);
    say("swp new=", m64);

    m32 = 0xfffffff0; // -16
    asm volatile("ldsmax %w1, %w0, [%2]" : "=r"(old) : "r"(3), "r"(&m32) : "memory");
    say("ldsmax(-16,3) -> ", m32);
    asm volatile("ldumin %w1, %w0, [%2]" : "=r"(old) : "r"(2), "r"(&m32) : "memory");
    say("ldumin(3,2) -> ", m32);
    m32 = 0xfffffff0;
    asm volatile("ldumax %w1, %w0, [%2]" : "=r"(old) : "r"(3), "r"(&m32) : "memory");
    say("ldumax(0xfffffff0,3) -> ", m32);

    m64 = 7;
    u64 cmp = 7;
    asm volatile("casal %0, %1, [%2]" : "+r"(cmp) : "r"(99UL), "r"(&m64) : "memory");
    say("cas hit: old=", cmp);
    say("         mem=", m64);
    cmp = 1;
    asm volatile("cas %0, %1, [%2]" : "+r"(cmp) : "r"(55UL), "r"(&m64) : "memory");
    say("cas miss: old=", cmp);
    say("          mem=", m64);
    u32 c32 = 0xfffffff0;
    m32 = 0xfffffff0;
    asm volatile("cas %w0, %w1, [%2]" : "+r"(c32) : "r"(1), "r"(&m32) : "memory");
    say("cas w hit: mem=", m32);

    // exclusives
    asm volatile("stxr %w0, %1, [%2]" : "=&r"(st) : "r"(1UL), "r"(&m64) : "memory");
    say("stxr without ldxr -> status ", st);
    asm volatile("ldxr %0, [%2]\n clrex\n stxr %w1, %0, [%2]" : "=&r"(old), "=&r"(st) : "r"(&m64) : "memory");
    say("stxr after clrex -> status ", st);
    asm volatile("ldaxr %0, [%2]\n add %0, %0, #1\n stlxr %w1, %0, [%2]" : "=&r"(old), "=&r"(st) : "r"(&m64) : "memory");
    say("ldaxr/stlxr status ", st);
    say("               mem=", m64);
    static volatile u64 pair[2] __attribute__((aligned(16))) = {1, 2};
    u64 lo, hi;
    asm volatile("ldxp %0, %1, [%3]\n stxp %w2, %1, %0, [%3]" : "=&r"(lo), "=&r"(hi), "=&r"(st) : "r"(pair) : "memory");
    say("ldxp/stxp status ", st);
    say("   swapped pair ", pair[0] << 8 | pair[1]);
    u64 acq;
    asm volatile("stlr %1, [%2]\n ldar %0, [%2]" : "=&r"(acq) : "r"(0xabcUL), "r"(&m64) : "memory");
    say("stlr/ldar ", acq);
}

int main(void) {
    puts("EL1: hello\n");
    say("CurrentEL=", mrs(currentel) >> 2);
    msr(vbar_el1, vectors);

    asm volatile("svc #0x42");
    asm volatile("brk #7");
    asm volatile(".word 0x00000000"); // UDF #0
    u64 v = 0;
    asm volatile("ldr %0, [%1]" : "+r"(v) : "r"(BAD_ADDR));
    asm volatile("str %0, [%1]" ::"r"(v), "r"(BAD_ADDR + 0x10));

    // SPSel / SP_EL0: with SPSel = 0 an exception goes to the "cur-EL SP0" vectors
    static char sp0_stack[1024] __attribute__((aligned(16)));
    msr(sp_el0, sp0_stack + sizeof sp0_stack);
    say("SP_EL0 offset=", mrs(sp_el0) - (u64)sp0_stack);
    asm volatile("msr spsel, #0\n svc #0x50\n msr spsel, #1" ::: "memory");
    say("SPSel=", mrs(spsel));

    // DAIF
    say("DAIF=", mrs(daif) >> 6);
    asm volatile("msr daifclr, #3");
    say("DAIF after daifclr #3=", mrs(daif) >> 6);
    asm volatile("msr daifset, #3");

    // NZCV round trip
    msr(nzcv, 0xa0000000UL);
    say("NZCV=", mrs(nzcv) >> 28);

    atomics();

    puts("EL1: entering EL0\n");
    enter_user((u64)user_main, (u64)(user_stack + sizeof user_stack));
    puts("EL1: back from EL0\n");
    say("shared=", shared);
    say("CurrentEL=", mrs(currentel) >> 2);
    puts("EL1: done\n");
    return 0;
}
