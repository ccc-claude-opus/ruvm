// AArch64 SMP test (4 CPUs): PSCI CPU_ON, exclusive-monitor counter and spinlock, LSE atomics,
// GICv2 SGIs between CPUs (with source CPU ids), TLBI broadcast.
// Output (hart 0 only) must be identical on qemu-system-aarch64 -smp 4 and ruvm -smp 4.
typedef unsigned long u64;
typedef unsigned int u32;
#define NC 4
#define UART ((volatile u32 *)0x09000000)
#define GICD 0x08000000UL
#define GICC 0x08010000UL
#define R32(a) (*(volatile u32 *)(a))
#define mrs(r) ({ u64 _v; asm volatile("mrs %0, " #r : "=r"(_v)); _v; })

extern char vectors[];
extern void secondary_entry(void);

static void putc(char c) { *UART = c; }
static void puts(const char *s) { while (*s) putc(*s++); }
static void puthex(u64 v) { puts("0x"); for (int i = 60; i >= 0; i -= 4) putc("0123456789abcdef"[(v >> i) & 15]); }
static void say(const char *s, u64 v) { puts(s); puthex(v); putc('\n'); }

volatile u32 counter, lse_counter, lockword, done[NC], started[NC], sgi_log[NC][4];
volatile u64 protected_count, mpidr_seen[NC];

static u64 psci(u64 fn, u64 a, u64 b, u64 c) {
    register u64 x0 asm("x0") = fn, x1 asm("x1") = a, x2 asm("x2") = b, x3 asm("x3") = c;
    asm volatile("hvc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3) : "memory");
    return x0;
}

static void lock(void) {
    u32 t, s;
    asm volatile("sevl\n 1: wfe\n 2: ldaxr %w0, [%2]\n cbnz %w0, 1b\n mov %w0, #1\n stxr %w1, %w0, [%2]\n cbnz %w1, 2b"
                 : "=&r"(t), "=&r"(s) : "r"(&lockword) : "memory");
}
static void unlock(void) { asm volatile("stlr wzr, [%0]\n sev" ::"r"(&lockword) : "memory"); }

// IRQ: acknowledge the SGI, record (id, source cpu) for this CPU
void exc_handler(u64 *f, int idx) {
    (void)f;
    if (idx != 5) { puts("unexpected vector\n"); for (;;) ; }
    u32 iar = R32(GICC + 0x0c);
    u32 id = iar & 0x3ff, src = (iar >> 10) & 7;
    u32 cpu = mrs(mpidr_el1) & 0xff;
    if (id < 16) sgi_log[cpu][src] = id + 1;
    R32(GICC + 0x10) = iar;
}

static void gic_cpu_init(void) {
    R32(GICC + 0x00) = 1;
    R32(GICC + 0x04) = 0xff;
    R32(GICD + 0x100) = 0xffff;             // ISENABLER0: SGIs 0..15 (banked)
    asm volatile("msr daifclr, #2\n isb" ::: "memory");
}

static void work(u32 cpu) {
    for (int i = 0; i < 500; i++) {
        u32 t, s;
        asm volatile("1: ldxr %w0, [%2]\n add %w0, %w0, #1\n stxr %w1, %w0, [%2]\n cbnz %w1, 1b"
                     : "=&r"(t), "=&r"(s) : "r"(&counter) : "memory");
    }
    for (int i = 0; i < 500; i++) {
        u32 one = 1, old;
        asm volatile("ldadd %w1, %w0, [%2]" : "=&r"(old) : "r"(one), "r"(&lse_counter) : "memory");
    }
    for (int i = 0; i < 300; i++) {
        lock();
        u64 v = protected_count;
        for (volatile int k = 0; k < 3; k++) ;
        protected_count = v + 1;
        unlock();
    }
    asm volatile("dsb ishst\n tlbi vmalle1is\n dsb ish\n isb" ::: "memory");   // broadcast TLBI
    mpidr_seen[cpu] = mrs(mpidr_el1) & 0xff;
    __atomic_store_n(&done[cpu], 1, __ATOMIC_SEQ_CST);
}

void secondary_main(u64 cpu) {
    gic_cpu_init();
    __atomic_store_n(&started[cpu], 1, __ATOMIC_SEQ_CST);
    work(cpu);
    for (;;) asm volatile("wfi");
}

int main(void) {
    asm volatile("msr vbar_el1, %0" ::"r"(vectors));
    puts("AArch64 SMP test\n");
    say("PSCI version     = ", psci(0x84000000, 0, 0, 0));
    say("CPU_ON(self)     = ", psci(0xC4000003, 0, (u64)secondary_entry, 0));   // already on: -4
    say("CPU_ON(bad)      = ", psci(0xC4000003, 9, (u64)secondary_entry, 0));   // invalid: -2
    say("AFFINITY_INFO(1) = ", psci(0xC4000004, 1, 0, 0));                      // OFF = 1
    R32(GICD + 0x000) = 1;
    gic_cpu_init();
    for (u64 c = 1; c < NC; c++) say("CPU_ON            = ", psci(0xC4000003, c, (u64)secondary_entry, c));
    work(0);
    for (int c = 0; c < NC; c++) while (!__atomic_load_n(&done[c], __ATOMIC_SEQ_CST)) ;
    say("AFFINITY_INFO(1) = ", psci(0xC4000004, 1, 0, 0));                      // ON = 0
    say("CPU_ON(running)  = ", psci(0xC4000003, 1, (u64)secondary_entry, 0));   // -4
    say("ldxr counter     = ", counter);
    say("ldadd counter    = ", lse_counter);
    say("locked counter   = ", protected_count);
    for (int c = 0; c < NC; c++) say("mpidr seen       = ", mpidr_seen[c]);
    // SGIs: CPU 0 -> CPU 1 (list), CPU 0 -> everyone else (filter 1), CPU 0 -> self (filter 2)
    R32(GICD + 0xf00) = (0u << 24) | (0x2u << 16) | 3;   // target list = CPU 1, SGI 3
    R32(GICD + 0xf00) = (1u << 24) | 5;                   // all but self, SGI 5
    R32(GICD + 0xf00) = (2u << 24) | 7;                   // self, SGI 7
    for (volatile int k = 0; k < 20000; k++) ;
    for (int c = 0; c < NC; c++) say("sgi from cpu0 -> cpu", c);
    for (int c = 0; c < NC; c++) say("  id seen (0=none) = ", sgi_log[c][0]);
    puts("done\n");
    psci(0x84000008, 0, 0, 0);
    for (;;) ;
}
