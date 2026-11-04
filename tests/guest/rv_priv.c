// RISC-V privileged-architecture test: M/S/U modes, trap delegation, Sv39,
// CLINT timer/software interrupts, PLIC + UART interrupt, MPRV/SUM/MXR.
// The output must be identical on qemu-system-riscv64 (-M virt -bios none) and ruvm.
// Built with -march=rv64imac, so it also exercises the C extension.

typedef unsigned long u64;
typedef unsigned int u32;

#define UART  ((volatile unsigned char *)0x10000000)
#define CLINT_MSIP     (*(volatile u32 *)0x2000000)
#define CLINT_MTIMECMP (*(volatile u64 *)0x2004000)
#define CLINT_MTIME    (*(volatile u64 *)0x200bff8)
#define PLIC(off)      (*(volatile u32 *)(0xc000000 + (off)))

#define csrr(r) ({ u64 _v; asm volatile("csrr %0, " #r : "=r"(_v)); _v; })
#define csrw(r, v) asm volatile("csrw " #r ", %0" ::"r"((u64)(v)))
#define csrs(r, v) asm volatile("csrs " #r ", %0" ::"r"((u64)(v)))
#define csrc(r, v) asm volatile("csrc " #r ", %0" ::"r"((u64)(v)))

struct frame { u64 x[32]; u64 epc; u64 pad; };

extern char user_code[], user_data[], user_exit[], s_trap_entry[];
extern void enter_user(u64 pc, u64 sp);
u64 kctx[4];

static void putc(char c) { *UART = c; }
static void puts(const char *s) { while (*s) putc(*s++); }
static void puthex(u64 v) {
    puts("0x");
    for (int i = 60; i >= 0; i -= 4) putc("0123456789abcdef"[(v >> i) & 15]);
}
static void say(const char *s, u64 v) { puts(s); puthex(v); putc('\n'); }

// ------------------------------------------------------------------ page tables

#define PTE_V 1
#define PTE_R 2
#define PTE_W 4
#define PTE_X 8
#define PTE_U 16
#define PTE_A 64
#define PTE_D 128

__attribute__((aligned(4096))) u64 root[512], l1[512], l0[512];
__attribute__((aligned(4096))) char ustack[4096];
__attribute__((aligned(16))) char m_stack[2048], s_stack[2048];
volatile u64 secret = 0x5ec2e7;

static u64 pte(u64 pa, u64 flags) { return (pa >> 12) << 10 | flags | PTE_V; }

static void build_page_tables(void) {
    root[0] = pte(0, PTE_R | PTE_W | PTE_A | PTE_D);                           // devices (1 GiB)
    root[2] = pte(0x80000000, PTE_R | PTE_W | PTE_X | PTE_A | PTE_D);          // RAM, identity
    root[256] = pte(0x80000000, PTE_R | PTE_A);                                // high alias, read-only
    root[1] = pte((u64)l1, 0);                                                 // user space at 1 GiB
    l1[0] = pte((u64)l0, 0);
    l0[0] = pte((u64)user_code, PTE_U | PTE_R | PTE_X);                        // A/D left clear
    l0[1] = pte((u64)user_data, PTE_U | PTE_R | PTE_W);
    l0[2] = pte((u64)ustack, PTE_U | PTE_R | PTE_W | PTE_A | PTE_D);
    l0[3] = pte((u64)user_data, PTE_X | PTE_A);                                // S, execute-only
}

// ------------------------------------------------------------------ M mode

static volatile int msoft_seen;

static int insn_len(u64 pc) { return (*(unsigned short *)pc & 3) == 3 ? 4 : 2; }

void m_trap(struct frame *f) {
    u64 cause = csrr(mcause);
    if ((long)cause < 0) {
        switch (cause & 0xff) {
        case 7: // machine timer: forward to S as STIP, mask until re-armed
            csrc(mie, 1 << 7);
            csrs(mip, 1 << 5);
            return;
        case 3:
            CLINT_MSIP = 0;
            msoft_seen = 1;
            puts("M: software interrupt\n");
            return;
        }
        say("M: unexpected interrupt ", cause);
        goto fail;
    }
    switch (cause) {
    case 11: // ecall from M
        puts("M: ecall from M\n");
        f->epc += 4;
        return;
    case 2:
        say("M: illegal instruction, mtval=", csrr(mtval));
        f->epc += 4;
        return;
    case 3:
        say("M: breakpoint from priv ", (csrr(mstatus) >> 11) & 3);
        f->epc += insn_len(f->epc);
        return;
    case 4: // QEMU reports misaligned AMOs as load-misaligned, the spec says 6
    case 6:
        say("M: misaligned AMO, mtval=", csrr(mtval) & 0xfff);
        f->epc += 4;
        return;
    case 9: // SBI-like calls from S
        f->epc += 4;
        switch (f->x[17]) {
        case 0: // set_timer(a0)
            CLINT_MTIMECMP = f->x[10];
            csrc(mip, 1 << 5);
            csrs(mie, 1 << 7);
            return;
        case 1:
            putc(f->x[10]);
            return;
        case 5: { // load through the S-mode page table with MPRV
            u64 va = f->x[10], v;
            csrs(mstatus, 1 << 17);
            asm volatile("ld %0, 0(%1)" : "=r"(v) : "r"(va));
            csrc(mstatus, 1 << 17);
            f->x[10] = v;
            return;
        }
        case 6: // raise a machine software interrupt
            CLINT_MSIP = 1;
            return;
        case 8: // shutdown
            *(volatile u32 *)0x100000 = 0x5555;
            for (;;) ;
        }
        say("M: bad SBI call ", f->x[17]);
        goto fail;
    }
    say("M: unexpected trap cause=", cause);
    say("   mepc=", f->epc);
    say("   mtval=", csrr(mtval));
fail:
    *(volatile u32 *)0x100000 = (1 << 16) | 0x3333;
    for (;;) ;
}

// ------------------------------------------------------------------ S mode

static u64 sbi(u64 nr, u64 a0) {
    register u64 a7 asm("a7") = nr;
    register u64 x10 asm("a0") = a0;
    asm volatile("ecall" : "+r"(x10) : "r"(a7) : "memory");
    return x10;
}

static volatile int ticks, ssoft, ext_irq;

void s_trap(struct frame *f) {
    u64 cause = csrr(scause);
    if ((long)cause < 0) {
        switch (cause & 0xff) {
        case 5:
            ticks++;
            sbi(0, ~0UL); // disarm
            return;
        case 1:
            ssoft++;
            csrc(sip, 2);
            return;
        case 9: {
            u32 id = PLIC(0x201004);
            say("S: external irq, claim=", id);
            say("S: uart IIR=", UART[2]);
            UART[1] = 0; // IER: no more interrupts
            PLIC(0x201004) = id;
            ext_irq++;
            return;
        }
        }
        say("S: unexpected interrupt ", cause);
        sbi(8, 0);
    }
    u64 from_user = !(csrr(sstatus) & (1 << 8));
    switch (cause) {
    case 8: // syscalls from U
        f->epc += 4;
        switch (f->x[17]) {
        case 0:
            f->epc = (u64)user_exit;
            csrs(sstatus, 1 << 8); // return to S
            return;
        case 1: putc(f->x[10]); return;
        case 2: say("U: ", f->x[10]); return;
        }
        break;
    case 2:
        say(from_user ? "S: illegal insn from U, stval=" : "S: illegal insn from S, stval=", csrr(stval));
        f->epc += 4;
        return;
    case 12:
        say("S: instruction page fault, stval=", csrr(stval));
        f->epc = f->x[1]; // return to caller
        return;
    case 13:
    case 15:
        puts(from_user ? "S: U " : "S: S ");
        puts(cause == 13 ? "load" : "store");
        say(" page fault, stval=", csrr(stval));
        f->epc += 4;
        return;
    }
    say("S: unexpected trap cause=", cause);
    say("   sepc=", f->epc);
    say("   stval=", csrr(stval));
    sbi(8, 0);
}

static u64 s_load(u64 va) {
    u64 v = 0xdead;
    asm volatile(".option push\n.option norvc\nld %0, 0(%1)\n.option pop" : "+r"(v) : "r"(va));
    return v;
}

void s_main(void) {
    puts("S: hello\n");
    csrw(sscratch, s_stack + sizeof s_stack);
    csrw(stvec, s_trap_entry);
    build_page_tables();
    csrw(satp, (8UL << 60) | ((u64)root >> 12));
    asm volatile("sfence.vma");
    say("S: satp mode=", csrr(satp) >> 60);
    say("S: secret via high alias=", s_load((u64)&secret - 0x80000000 + 0xffffffc000000000UL));
    say("S: M-mode MPRV load=", sbi(5, (u64)&secret - 0x80000000 + 0xffffffc000000000UL));

    // non-canonical address
    s_load(0x0000008000000000UL);
    // write to the read-only alias
    asm volatile(".option push\n.option norvc\nsd zero, 0(%0)\n.option pop" ::"r"(0xffffffc000000000UL));
    // S touching a U page: fault without SUM, fine with it
    say("S: U page without SUM -> ", s_load(0x40001000));
    csrs(sstatus, 1 << 18);
    say("S: U page with SUM    -> ", s_load(0x40001000));
    csrc(sstatus, 1 << 18);
    // execute-only page: load faults unless MXR
    say("S: X-only page        -> ", s_load(0x40003000));
    csrs(sstatus, 1 << 19);
    say("S: X-only page + MXR  -> ", s_load(0x40003000));
    csrc(sstatus, 1 << 19);
    // S may not execute U pages
    ((void (*)(void))0x40000000)();
    // S-mode illegal instruction (M-only CSR), delegated to S
    asm volatile(".option push\n.option norvc\ncsrr t0, mstatus\n.option pop" ::: "t0");
    // ebreak is not delegated: goes to M
    asm volatile("ebreak");
    // misaligned AMO: not delegated, goes to M
    asm volatile(".option push\n.option norvc\namoadd.w zero, zero, (%0)\n.option pop" ::"r"((u64)&secret + 1));

    say("S: user A/D before = ", l0[0] & 0xc0);
    puts("S: entering U mode\n");
    enter_user(0x40000000, 0x40003000);
    puts("S: back from U mode\n");
    say("S: user code PTE A/D = ", l0[0] & 0xc0);
    say("S: user data PTE A/D = ", l0[1] & 0xc0);

    // supervisor software interrupt
    csrs(sie, 2);
    csrs(sstatus, 2);
    csrs(sip, 2);
    asm volatile("nop");
    say("S: software interrupts taken=", ssoft);

    // timer interrupt via SBI set_timer + M forwarding STIP
    csrs(sie, 1 << 5);
    u64 t0 = csrr(time);
    sbi(0, t0 + 1000);
    while (!ticks) asm volatile("wfi");
    say("S: timer ticks=", ticks);
    say("S: time advanced=", csrr(time) > t0);

    // machine software interrupt raised from S
    sbi(6, 0);
    say("S: M soft irq seen=", msoft_seen);

    // PLIC: UART (source 10) -> context 1 (S mode)
    PLIC(4 * 10) = 1;
    PLIC(0x2080) = 1 << 10;
    PLIC(0x201000) = 0;
    csrs(sie, 1 << 9);
    UART[1] = 2; // IER: THR empty interrupt
    for (int i = 0; i < 100 && !ext_irq; i++) asm volatile("nop");
    say("S: external irqs=", ext_irq);
    say("S: PLIC pending after complete=", PLIC(0x1000) & (1 << 10));

    puts("S: done\n");
    sbi(8, 0);
}

// ------------------------------------------------------------------ boot (M mode)

int main(void) {
    puts("M: hello\n");
    // PMP: give S/U access to everything (QEMU denies S/U accesses when PMP is empty)
    csrw(pmpaddr0, ~0UL >> 10);
    csrw(pmpcfg0, 0x1f);
    asm volatile("ecall");
    asm volatile(".option push\n.option norvc\ncsrw mhartid, zero\n.option pop");
    say("M: mhartid=", csrr(mhartid));
    say("M: misa has S,U = ", (csrr(misa) >> 18 & 1) + (csrr(misa) >> 20 & 1));

    csrw(menvcfg, 1UL << 61); // ADUE: hardware A/D updates (QEMU Svadu)
    csrw(mcounteren, ~0UL);
    csrw(medeleg, (1 << 2) | (1 << 8) | (1 << 12) | (1 << 13) | (1 << 15));
    csrw(mideleg, (1 << 1) | (1 << 5) | (1 << 9));
    say("M: mideleg=", csrr(mideleg) & 0x222); // QEMU also forces hypervisor bits
    csrs(mie, (1 << 3));
    // mret into S mode at s_main
    u64 ms = csrr(mstatus);
    ms = (ms & ~(3UL << 11)) | (1UL << 11);
    csrw(mstatus, ms);
    csrw(mepc, s_main);
    asm volatile("mret");
    return 0;
}
