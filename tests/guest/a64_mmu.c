// AArch64 stage-1 MMU test: 4K granule, 48-bit VA, TTBR0 + TTBR1, blocks and pages,
// translation / access-flag / permission faults at each level, execute-never, page-crossing
// accesses, TBI, LDTR/STTR, DC ZVA, TLBI, EL0 faults.  The output must be identical on
// qemu-system-aarch64 (-M virt -cpu max) and ruvm.

typedef unsigned long u64;
typedef unsigned int u32;
#define UART ((volatile u32 *)0x09000000)
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

// ---------------------------------------------------------------- page tables
#define VALID 1
#define TABLE 2
#define PAGE 2            // L3 page = 0b11
#define AF (1UL << 10)
#define SH_IS (3UL << 8)
#define AP_RW_EL1 (0UL << 6)
#define AP_RW_ALL (1UL << 6)
#define AP_RO_EL1 (2UL << 6)
#define AP_RO_ALL (3UL << 6)
#define NG (1UL << 11)
#define PXN (1UL << 53)
#define UXN (1UL << 54)
#define ATTR_NORMAL (1UL << 2)   // MAIR index 1
#define ATTR_DEVICE (0UL << 2)   // MAIR index 0

__attribute__((aligned(4096))) u64 l0[512], l1[512], l2[512], l3[512], l3k[512], h0[512], h1[512], h2[512];
__attribute__((aligned(4096))) char ucode[4096];   // EL0 program is copied here (executable at EL0 only)
__attribute__((aligned(4096))) char data[4 * 4096];
__attribute__((aligned(4096))) char user_stack[4096];
static char *const win = (char *)0x60000000;  // 2 MiB-aligned test window (VA == PA)

static u64 tbl(u64 *t) { return (u64)t | VALID | TABLE; }
static u64 page(u64 pa, u64 attrs) { return pa | VALID | PAGE | AF | SH_IS | ATTR_NORMAL | attrs; }
static u64 block(u64 pa, u64 attrs) { return pa | VALID | AF | SH_IS | ATTR_NORMAL | attrs; }

// ---------------------------------------------------------------- exceptions
static volatile int fault_count;
static u64 last_esr, last_far;

void exc_handler(u64 *f, int idx) {
    u64 esr = mrs(esr_el1), elr = mrs(elr_el1);
    u32 ec = esr >> 26;
    last_esr = esr;
    last_far = mrs(far_el1);
    fault_count++;
    puts(idx >= 8 ? "  [EL0] " : "  [EL1] ");
    if (ec == 0x15) { // SVC from EL0
        switch (esr & 0xffff) {
        case 0: msr(elr_el1, user_exit); msr(spsr_el1, 0x3c5); say("svc exit", 0); return;
        case 1: putc(f[0]); putc('\n'); return;
        }
    }
    say("EC=", ec);
    if (ec == 0x24 || ec == 0x25 || ec == 0x20 || ec == 0x21) {
        say("    FSC=", esr & 0x3f);
        say("    WnR=", (esr >> 6) & 1);
        say("    FAR=", last_far);
    }
    if (ec == 0x20 || ec == 0x21) {
        // instruction abort: skip by returning to the caller (x30)
        msr(elr_el1, f[30]);
    } else {
        msr(elr_el1, elr + 4);
    }
}

// access helpers (one instruction each so ELR + 4 skips exactly the access)
static u64 ld64(u64 va) { u64 v = 0x1111; asm volatile("ldr %0, [%1]" : "+r"(v) : "r"(va) : "memory"); return v; }
static void st64(u64 va, u64 v) { asm volatile("str %0, [%1]" ::"r"(v), "r"(va) : "memory"); }
static u64 ld8(u64 va) { u64 v = 0x22; asm volatile("ldrb %w0, [%1]" : "+r"(v) : "r"(va) : "memory"); return v; }
static u64 ldtr(u64 va) { u64 v = 0x33; asm volatile("ldtr %0, [%1]" : "+r"(v) : "r"(va) : "memory"); return v; }
static void sttr(u64 va, u64 v) { asm volatile("sttr %0, [%1]" ::"r"(v), "r"(va) : "memory"); }
static void call(u64 va) { asm volatile("blr %0" ::"r"(va) : "x30", "memory"); }

static void tlbi_all(void) { asm volatile("dsb ishst\n tlbi vmalle1is\n dsb ish\n isb" ::: "memory"); }

// ---------------------------------------------------------------- EL0 program
void user_main(void) {
    register u64 x0 asm("x0");
    x0 = 'U'; asm volatile("svc #1" : "+r"(x0));
    // EL0 may use the user page, not the EL1-only one
    volatile u64 *p = (volatile u64 *)(win + 0x1000);
    *p = 0x5eed;
    x0 = *p == 0x5eed ? 'k' : 'x'; asm volatile("svc #1" : "+r"(x0));
    asm volatile("ldr %0, [%1]" : "=r"(x0) : "r"(win + 0x2000)); // EL1-only page: permission fault
    asm volatile("str %0, [%1]" ::"r"(x0), "r"(win + 0x3000));   // read-only page: permission fault
    asm volatile("ldr %0, [%1]" : "=r"(x0) : "r"(win + 0x9000)); // unmapped: translation fault L3
    asm volatile("svc #0");
}

int main(void) {
    puts("EL1: mmu test\n");
    msr(vbar_el1, vectors);

    // ---- build tables: TTBR0 maps devices (L1[0] block), RAM via L2 2MiB blocks, and an
    // L3 table over the 2MiB window at 0x60000000
    l0[0] = tbl(l1);
    l1[0] = block(0, ATTR_DEVICE) | UXN | PXN;           // 0..1GiB device
    l1[1] = tbl(l2);                                      // 1..2GiB RAM
    for (int k = 0; k < 64; k++) l2[k] = block(0x40000000UL + (u64)k * 0x200000, AP_RW_EL1);  // 0x40000000.. (128 MiB)
    // first 2 MiB (image, data, stacks) as pages: EL1 only, except the EL0 stack page
    l2[0] = tbl(l3k);
    for (int k = 0; k < 512; k++) l3k[k] = page(0x40000000UL + (u64)k * 4096, AP_RW_EL1);
    l3k[((u64)user_stack - 0x40000000UL) >> 12] = page((u64)user_stack, AP_RW_ALL | UXN | PXN);
    l2[(0x60000000UL - 0x40000000UL) >> 21] = tbl(l3);   // window: pages
    l2[(0x60200000UL - 0x40000000UL) >> 21] = block(0x60200000UL, AP_RW_ALL | UXN | PXN);      // 2MiB block, EL0 ok
    l2[(0x60400000UL - 0x40000000UL) >> 21] = block(0x60400000UL, AP_RW_EL1 | UXN | PXN) & ~AF; // AF = 0
    l2[(0x60600000UL - 0x40000000UL) >> 21] = block(0x60600000UL, AP_RO_EL1 | UXN | PXN);      // RO block
    // window pages (physical backing = data[], 4 pages, reused)
    u64 d0 = (u64)data;
    l3[0] = page(d0 + 0x0000, AP_RW_EL1 | UXN | PXN);             // EL1 rw
    l3[1] = page(d0 + 0x1000, AP_RW_ALL | UXN | PXN);             // EL0 rw
    l3[2] = page(d0 + 0x2000, AP_RW_EL1 | UXN | PXN);             // EL1 only
    l3[3] = page(d0 + 0x3000, AP_RO_ALL | UXN | PXN);             // read-only for all
    l3[5] = page(d0 + 0x1000, AP_RW_EL1 | UXN | PXN) & ~AF;      // AF = 0
    l3[6] = page(d0 + 0x2000, AP_RW_EL1 | UXN);                   // PXN clear, UXN set: EL1 may execute
    l3[7] = page(d0 + 0x2000, AP_RW_EL1 | PXN | UXN);             // PXN: no execute at EL1
    l3[4] = page((u64)ucode, AP_RO_ALL | PXN);                    // EL0 code (r-x at EL0, not executable at EL1)
    // l3[8] unmapped; l3[9] left to test unmapped-in-table
    // TTBR1 alias of the window at 0xffff000060000000 -> L0[0] of its own table, L1[1]... (VA bits)
    h0[0] = tbl(h1);
    h1[1] = tbl(h2);                                      // 0xffff000040000000..
    h2[(0x60000000UL - 0x40000000UL) >> 21] = tbl(l3);    // same window pages through TTBR1
    for (int k = 0; k < 64; k++) if (h2[k] == 0) h2[k] = block(0x40000000UL + (u64)k * 0x200000, AP_RW_EL1);

    // MAIR: idx0 = Device-nGnRnE, idx1 = Normal WBWA
    msr(mair_el1, 0xffUL << 8);
    // TCR: T0SZ=16, T1SZ=16, 4K granules (TG0=0, TG1=2), inner/outer WBWA, IS, IPS=40bit
    u64 tcr = 16 | 16UL << 16 | 2UL << 30 | 1UL << 8 | 1UL << 10 | 3UL << 12 | 1UL << 24 | 1UL << 26 | 3UL << 28 | 2UL << 32;
    msr(tcr_el1, tcr);
    msr(ttbr0_el1, l0);
    msr(ttbr1_el1, h0);
    asm volatile("isb");
    msr(sctlr_el1, (mrs(sctlr_el1) | 1 | (1 << 2) | (1 << 12)) & ~(1UL << 20)); // M, C, I; UWXN off
    asm volatile("isb");
    puts("MMU on\n");

    volatile u64 *w = (volatile u64 *)win;
    // 1. plain access through TTBR0 and the TTBR1 alias
    st64((u64)win, 0xa1b2c3d4e5f60718UL);
    say("TTBR0 read:   ", ld64((u64)win));
    say("TTBR1 alias:  ", ld64(0xffff000060000000UL));
    st64(0xffff000060000008UL, 0x1234);
    say("via TTBR0:    ", ld64((u64)win + 8));

    // 2. unaligned and page-crossing accesses
    st64((u64)win + 0xffc, 0x0102030405060708UL);   // crosses page 0 -> page 1
    say("cross read:   ", ld64((u64)win + 0xffc));
    say("byte in p1:   ", ld8((u64)win + 0x1000));
    st64((u64)win + 0x2ffc, 0xdeadbeefUL); // page 2 EL1-only is fine at EL1; crosses into page 3 (RO) -> fault
    say("page3 byte0:  ", ld8((u64)win + 0x3000));

    // 3. translation faults at every level
    puts("-- translation faults\n");
    ld64((u64)win + 0x9000);            // L3 entry invalid
    ld64(0x80000000UL);                 // L1 entry invalid: level 1
    ld64(0x60800000UL);                 // L2 entry invalid: level 2
    ld64(1UL << 39);                    // L0 entry invalid: level 0
    ld64(0x0001000000000000UL);         // outside both regions (bits 63:48 mixed): level 0

    // 4. access flag faults
    puts("-- access flag\n");
    ld64((u64)win + 0x5000);            // L3 AF=0
    ld64(0x60400000UL);                 // L2 block AF=0
    // 5. permission faults
    puts("-- permissions\n");
    st64((u64)win + 0x3000, 1);         // RO page
    st64(0x60600000UL, 1);              // RO block
    say("RO block read: ", ld64(0x60600000UL) & 0);
    // execute never: a blr into data pages must fault as instruction aborts
    puts("-- execute never\n");
    call((u64)win + 0x7000);            // PXN page
    call((u64)win + 0x1000);            // UXN|PXN
    call(0x60200000UL);                 // block PXN
    // 6. unprivileged accesses at EL1
    puts("-- LDTR/STTR\n");
    st64((u64)win + 0x1000, 0xcafe);
    say("ldtr user pg:  ", ldtr((u64)win + 0x1000));
    say("ldtr kern pg:  ", ldtr((u64)win + 0x2000));   // EL1-only page: faults as EL0
    sttr((u64)win + 0x3000, 5);                           // RO
    // 7. TLBI + remap
    puts("-- remap\n");
    st64((u64)win + 0x0000, 0x1111);
    st64((u64)win + 0x1000, 0x2222);
    say("page0:         ", ld64((u64)win));
    l3[0] = page(d0 + 0x1000, AP_RW_EL1 | UXN | PXN);
    tlbi_all();
    say("page0 remapped:", ld64((u64)win));
    l3[0] = 0;
    tlbi_all();
    ld64((u64)win);                     // now unmapped
    l3[0] = page(d0 + 0x0000, AP_RW_EL1 | UXN | PXN);
    tlbi_all();
    say("page0 back:    ", ld64((u64)win));
    // 8. top-byte ignore
    puts("-- TBI\n");
    ld64((u64)win | (0x5aUL << 56));    // faults without TBI0
    msr(tcr_el1, tcr | 1UL << 37);
    asm volatile("isb");
    tlbi_all();
    say("tagged read:   ", ld64((u64)win | (0x5aUL << 56)));
    msr(tcr_el1, tcr);
    asm volatile("isb");
    tlbi_all();
    // 9. DC ZVA
    puts("-- DC ZVA\n");
    for (int k = 0; k < 16; k++) w[k] = 0xffffffffffffffffUL;
    asm volatile("dc zva, %0" ::"r"((u64)win + 0x28) : "memory"); // zeroes the 64-byte block at win+0
    say("zva w[0]:      ", w[0]);
    say("zva w[7]:      ", w[7]);
    say("zva w[8]:      ", w[8]);
    // 10. EL0
    puts("-- EL0\n");
    for (int k = 0; k < 256; k++) ucode[k] = ((char *)user_main)[k];   // position-independent: no ADRP / calls
    asm volatile("dc cvau, %0\n ic ialluis\n dsb ish\n isb" ::"r"(ucode) : "memory");
    enter_user((u64)win + 0x4000, (u64)(user_stack + sizeof user_stack));
    puts("EL1: back\n");
    say("faults: ", fault_count);
    // 11. back to MMU-off
    msr(sctlr_el1, mrs(sctlr_el1) & ~1UL);
    asm volatile("isb");
    puts("MMU off\n");
    return 0;
}
