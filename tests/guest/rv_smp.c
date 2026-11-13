// RISC-V SMP test (4 harts): AMO counter, LR/SC spinlock, CLINT software interrupts.
// Output (printed by hart 0 only) must be identical on qemu-system-riscv64 -smp 4 and ruvm -smp 4.
typedef unsigned long u64;
typedef unsigned int u32;
#define NH 4
#define UART ((volatile unsigned char *)0x10000000)
#define MSIP(h) (*(volatile u32 *)(0x2000000UL + 4UL * (h)))
#define csrr(r) ({ u64 _v; asm volatile("csrr %0, " #r : "=r"(_v)); _v; })
#define csrs(r, v) asm volatile("csrs " #r ", %0" ::"r"((u64)(v)))

static void putc(char c) { *UART = c; }
static void puts(const char *s) { while (*s) putc(*s++); }
static void puthex(u64 v) { puts("0x"); for (int i = 60; i >= 0; i -= 4) putc("0123456789abcdef"[(v >> i) & 15]); }
static void say(const char *s, u64 v) { puts(s); puthex(v); putc('\n'); }

volatile u32 counter, lockword, ipi_got[NH], done[NH], started[NH], acked[NH];
volatile u64 protected_count;
volatile u64 hart_seen[NH];

static void lock(void) {
    u32 tmp;
    asm volatile("1: lr.w.aq %0, (%1)\n bnez %0, 1b\n li %0, 1\n sc.w %0, %0, (%1)\n bnez %0, 1b"
                 : "=&r"(tmp) : "r"(&lockword) : "memory");
}
static void unlock(void) { asm volatile("fence rw, w\n sw zero, 0(%0)" ::"r"(&lockword) : "memory"); }

void main(u64 hart) {
    hart_seen[hart] = csrr(mhartid) + 100;
    asm volatile("fence rw, rw");
    __atomic_fetch_add(&started[hart], 1, __ATOMIC_SEQ_CST);
    csrs(mie, 1 << 3);       // MSIE
    csrs(mstatus, 1 << 3);   // MIE

    for (int i = 0; i < 1000; i++) __atomic_fetch_add(&counter, 1, __ATOMIC_RELAXED);
    for (int i = 0; i < 400; i++) {
        lock();
        u64 v = protected_count;   // deliberately non-atomic read-modify-write under the lock
        for (volatile int k = 0; k < 3; k++) ;
        protected_count = v + 1;
        unlock();
    }
    __atomic_store_n(&done[hart], 1, __ATOMIC_SEQ_CST);

    if (hart != 0) {
        while (!ipi_got[hart]) asm volatile("wfi");
        __atomic_store_n(&acked[hart], 1, __ATOMIC_SEQ_CST);
        for (;;) asm volatile("wfi");
    }
    // hart 0 coordinates and reports
    for (int h = 0; h < NH; h++) while (!__atomic_load_n(&done[h], __ATOMIC_SEQ_CST)) ;
    puts("RISC-V SMP test\n");
    say("AMO counter      = ", counter);
    say("locked counter   = ", protected_count);
    for (int h = 0; h < NH; h++) say("hart seen        = ", hart_seen[h]);
    for (int h = 1; h < NH; h++) MSIP(h) = 1;        // software interrupt to every other hart
    for (int h = 1; h < NH; h++) while (!__atomic_load_n(&acked[h], __ATOMIC_SEQ_CST)) ;
    for (int h = 1; h < NH; h++) say("ipi acknowledged = ", ipi_got[h] + 10 * acked[h]);
    puts("done\n");
    *(volatile u32 *)0x100000 = 0x5555;
    for (;;) ;
}
