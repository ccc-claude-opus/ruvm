// AArch64 interrupt test: GICv2 distributor + CPU interface, generic virtual timer (PPI 27),
// SGIs with priorities, PL011 TX interrupt (SPI 1 = INTID 33), WFI, DAIF masking.
// Output must be identical on qemu-system-aarch64 (-M virt -cpu max) and ruvm.

typedef unsigned long u64;
typedef unsigned int u32;
#define UART_BASE 0x09000000UL
#define GICD 0x08000000UL
#define GICC 0x08010000UL
#define R32(a) (*(volatile u32 *)(a))
#define mrs(r) ({ u64 _v; asm volatile("mrs %0, " #r : "=r"(_v)); _v; })
#define msr(r, v) asm volatile("msr " #r ", %0" ::"r"((u64)(v)))

extern char vectors[];

static void putc(char c) { R32(UART_BASE) = c; }
static void puts(const char *s) { while (*s) putc(*s++); }
static void puthex(u64 v) { puts("0x"); for (int i = 60; i >= 0; i -= 4) putc("0123456789abcdef"[(v >> i) & 15]); }
static void say(const char *s, u64 v) { puts(s); puthex(v); putc('\n'); }

static volatile u32 seen[8];
static volatile int nseen;
static volatile int mask_uart_in_handler;

// IRQ vector index: 5 = current EL SPx IRQ
void exc_handler(u64 *f, int idx) {
    (void)f;
    if (idx != 5) {
        say("unexpected vector ", idx);
        R32(0x09000000UL + 0x44) = 0x7ff;
        asm volatile("ldr x0, =0x84000008\n hvc #0");
    }
    u32 iar = R32(GICC + 0x0c);
    u32 id = iar & 0x3ff;
    if (id == 27) {
        msr(cntv_ctl_el0, 2);              // mask the timer output
    } else if (id == 33) {
        R32(UART_BASE + 0x38) = 0;         // IMSC = 0
        R32(UART_BASE + 0x44) = 0x7ff;     // ICR: clear everything
    }
    if (nseen < 8) seen[nseen++] = id;
    R32(GICC + 0x10) = iar;                // EOIR
}

static void irq_on(void) { asm volatile("msr daifclr, #2\n isb" ::: "memory"); }
static void irq_off(void) { asm volatile("msr daifset, #2\n isb" ::: "memory"); }

static void report(const char *what) {
    puts(what);
    for (int k = 0; k < nseen; k++) say(" id=", seen[k]);
    nseen = 0;
}

int main(void) {
    puts("EL1: irq test\n");
    msr(vbar_el1, vectors);
    // GIC: enable distributor and CPU interface, accept all priorities
    R32(GICD + 0x000) = 1;
    R32(GICC + 0x00) = 1;
    R32(GICC + 0x04) = 0xff;
    say("PMR readback = ", R32(GICC + 0x04));
    say("pending now  = ", R32(GICC + 0x18) & 0x3ff);   // HPPIR: 1023 = nothing pending

    // --- virtual timer (PPI 27)
    R32(GICD + 0x100) = 1u << 27;                        // ISENABLER0: PPI 27
    R32(GICD + 0x400 + 24) = 0x40404040;                 // IPRIORITYR6: INTIDs 24..27
    msr(cntv_tval_el0, 2000);
    msr(cntv_ctl_el0, 1);
    say("timer CTL    = ", mrs(cntv_ctl_el0) & 3);       // enabled, not masked, not yet fired
    irq_on();
    while (nseen == 0) asm volatile("wfi");
    irq_off();
    report("timer fired:");
    say("timer CTL    = ", mrs(cntv_ctl_el0));           // ENABLE | IMASK | ISTATUS
    msr(cntv_ctl_el0, 0);

    // --- SGIs: ids 1 (prio 0x80) and 2 (prio 0x20), raised while IRQs are masked
    R32(GICD + 0x100) |= 1 << 1 | 1 << 2;
    R32(GICD + 0x400) = 0x20208000 | 0x00000000;         // IPRIORITYR0: id0 0x00, id1 0x80, id2 0x20, id3 0x20
    R32(GICD + 0x400) = (0x20u << 24) | (0x20u << 16) | (0x80u << 8) | 0x00;
    R32(GICD + 0xf00) = (2u << 24) | 1;                  // SGIR: target = self, id 1
    R32(GICD + 0xf00) = (2u << 24) | 2;                  // id 2
    say("HPPIR        = ", R32(GICC + 0x18) & 0x3ff);    // highest priority pending: 2
    irq_on();
    for (int k = 0; k < 100; k++) asm volatile("nop");
    irq_off();
    report("SGIs taken:");

    // --- priority mask blocks lower-priority interrupts
    R32(GICC + 0x04) = 0x40;                             // PMR: only priority < 0x40 pass
    R32(GICD + 0xf00) = (2u << 24) | 1;                  // id 1 has priority 0x80: blocked
    R32(GICD + 0xf00) = (2u << 24) | 2;                  // id 2 has priority 0x20: passes
    irq_on();
    for (int k = 0; k < 100; k++) asm volatile("nop");
    irq_off();
    report("with PMR=0x40:");
    say("HPPIR        = ", R32(GICC + 0x18) & 0x3ff);    // id 1 still pending but masked by PMR -> 1023
    R32(GICC + 0x04) = 0xff;
    irq_on();
    for (int k = 0; k < 100; k++) asm volatile("nop");
    irq_off();
    report("PMR back to 0xff:");

    // --- PL011 transmit interrupt (SPI 1 -> INTID 33)
    R32(GICD + 0x104) = 1u << (33 - 32);                 // ISENABLER1
    R32(GICD + 0x400 + 32) = 0x80808080;                 // priorities for 32..35
    R32(UART_BASE + 0x38) = 1 << 5;                      // IMSC: TXIM
    putc('T');                                           // a transmit raises TXRIS
    irq_on();
    while (nseen == 0) asm volatile("wfi");
    irq_off();
    putc('\n');
    report("uart irq:");
    say("UART RIS     = ", R32(UART_BASE + 0x3c) & 0x70);
    say("UART MIS     = ", R32(UART_BASE + 0x40));
    say("PL011 PID0   = ", R32(UART_BASE + 0xfe0));
    say("PL011 CID3   = ", R32(UART_BASE + 0xffc));

    // --- masked IRQs stay pending until unmasked; disabling the GIC blocks delivery
    R32(GICD + 0x000) = 0;
    R32(GICD + 0xf00) = (2u << 24) | 2;
    irq_on();
    for (int k = 0; k < 100; k++) asm volatile("nop");
    irq_off();
    report("distributor off:");
    R32(GICD + 0x000) = 1;
    irq_on();
    for (int k = 0; k < 100; k++) asm volatile("nop");
    irq_off();
    report("distributor on:");
    puts("EL1: done\n");
    return 0;
}
