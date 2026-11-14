// User-mode test program (ruvm -user): freestanding, raw Linux syscalls, no libc.
// Prints what it sees so the output can be compared against a fixed expectation.
typedef unsigned long u64;
typedef long i64;

#if defined(__riscv)
#define SYSCALL_ASM(nr, a0, a1, a2, a3)                                                              \
    register u64 r_a0 __asm__("a0") = a0; register u64 r_a1 __asm__("a1") = a1;                       \
    register u64 r_a2 __asm__("a2") = a2; register u64 r_a3 __asm__("a3") = a3;                       \
    register u64 r_nr __asm__("a7") = nr;                                                             \
    __asm__ volatile("ecall" : "+r"(r_a0) : "r"(r_a1), "r"(r_a2), "r"(r_a3), "r"(r_nr) : "memory");   \
    return r_a0;
__asm__(".globl _start\n_start:\n mv a0, sp\n andi sp, sp, -16\n call start_c\n");
#else
#define SYSCALL_ASM(nr, a0, a1, a2, a3)                                                              \
    register u64 r_a0 __asm__("x0") = a0; register u64 r_a1 __asm__("x1") = a1;                       \
    register u64 r_a2 __asm__("x2") = a2; register u64 r_a3 __asm__("x3") = a3;                       \
    register u64 r_nr __asm__("x8") = nr;                                                             \
    __asm__ volatile("svc #0" : "+r"(r_a0) : "r"(r_a1), "r"(r_a2), "r"(r_a3), "r"(r_nr) : "memory");  \
    return r_a0;
__asm__(".globl _start\n_start:\n mov x0, sp\n and sp, x0, #-16\n bl start_c\n");
#endif

static i64 sys(u64 nr, u64 a, u64 b, u64 c, u64 d) { SYSCALL_ASM(nr, a, b, c, d) }
static i64 sys6(u64 nr, u64 a, u64 b, u64 c, u64 d, u64 e, u64 f);

static u64 len(const char *s) { u64 n = 0; while (s[n]) n++; return n; }
static void out(const char *s) { sys(64, 1, (u64)s, len(s), 0); }
static void num(i64 v) {
    char b[24]; int i = 23; b[i] = 0; int neg = v < 0; u64 u = neg ? -v : v;
    do { b[--i] = '0' + u % 10; u /= 10; } while (u);
    if (neg) b[--i] = '-';
    out(b + i);
}
static void line(const char *k, i64 v) { out(k); out("="); num(v); out("\n"); }

#if defined(__riscv)
static i64 sys6(u64 nr, u64 a, u64 b, u64 c, u64 d, u64 e, u64 f) {
    register u64 r0 __asm__("a0") = a; register u64 r1 __asm__("a1") = b; register u64 r2 __asm__("a2") = c;
    register u64 r3 __asm__("a3") = d; register u64 r4 __asm__("a4") = e; register u64 r5 __asm__("a5") = f;
    register u64 r7 __asm__("a7") = nr;
    __asm__ volatile("ecall" : "+r"(r0) : "r"(r1), "r"(r2), "r"(r3), "r"(r4), "r"(r5), "r"(r7) : "memory");
    return r0;
}
#else
static i64 sys6(u64 nr, u64 a, u64 b, u64 c, u64 d, u64 e, u64 f) {
    register u64 r0 __asm__("x0") = a; register u64 r1 __asm__("x1") = b; register u64 r2 __asm__("x2") = c;
    register u64 r3 __asm__("x3") = d; register u64 r4 __asm__("x4") = e; register u64 r5 __asm__("x5") = f;
    register u64 r8 __asm__("x8") = nr;
    __asm__ volatile("svc #0" : "+r"(r0) : "r"(r1), "r"(r2), "r"(r3), "r"(r4), "r"(r5), "r"(r8) : "memory");
    return r0;
}
#endif

void start_c(u64 *sp) {
    i64 argc = sp[0];
    char **argv = (char **)(sp + 1);
    out("hello from user mode\n");
    if (argc > 1 && argv[1][0] == 'n' && argv[1][1] == 'u') *(volatile int *)8 = 1; // null-pointer access must fault
    line("argc", argc);
    for (i64 i = 1; i < argc; i++) { out("arg: "); out(argv[i]); out("\n"); }

    // auxv: AT_PAGESZ and AT_RANDOM must be there
    char **envp = argv + argc + 1;
    while (*envp) envp++;
    u64 *aux = (u64 *)(envp + 1), pagesz = 0, random = 0;
    for (; aux[0]; aux += 2) { if (aux[0] == 6) pagesz = aux[1]; if (aux[0] == 25) random = aux[1]; }
    line("pagesz", pagesz);
    line("random", random != 0);

    // brk heap
    u64 b0 = sys(214, 0, 0, 0, 0);
    u64 b1 = sys(214, b0 + 3 * 4096, 0, 0, 0);
    line("brk grew", (i64)(b1 - b0));
    ((volatile char *)b0)[3 * 4096 - 1] = 7;

    // anonymous mmap: zero filled, writable, then unmapped
    volatile unsigned *m = (unsigned *)sys6(222, 0, 1 << 20, 3, 0x22, -1, 0);
    u64 sum = 0;
    for (int i = 0; i < (1 << 18); i += 4096) sum += m[i];
    for (int i = 0; i < (1 << 18); i++) m[i] = i;
    for (int i = 0; i < (1 << 18); i++) sum += m[i];
    line("mmap sum", sum);
    line("munmap", sys(215, (u64)m, 1 << 20, 0, 0));

    // floating point
    volatile double x = 2.0, y = x;
    for (int i = 0; i < 20; i++) y = (y + x / y) / 2;
    line("sqrt2 * 1e9", (i64)(y * 1e9));

    // files: read our own ELF header through /proc/self/exe
    i64 fd = sys6(56, (u64)-100, (u64)"/proc/self/exe", 0, 0, 0, 0);
    char hdr[8] = {0};
    sys(63, fd, (u64)hdr, 4, 0);
    out("magic: "); out(hdr + 1); out("\n");
    sys(57, fd, 0, 0, 0);
    line("open missing", sys6(56, (u64)-100, (u64)"/nonexistent/file", 0, 0, 0, 0));

    // uname and clock
    char un[65 * 6];
    sys(160, (u64)un, 0, 0, 0);
    out("sysname: "); out(un); out("\nmachine: "); out(un + 65 * 4); out("\n");
    u64 ts[2];
    sys(113, 0, (u64)ts, 0, 0);
    line("clock ok", ts[0] > 1600000000);

    // pipe: bytes written come out of the read end; a closed reader gives EPIPE; an empty pipe would block
    int pfd[2];
    line("pipe2", sys(59, (u64)pfd, 0, 0, 0));
    sys(64, pfd[1], (u64)"ping", 4, 0);
    char pb[8] = {0};
    line("pipe read", sys(63, pfd[0], (u64)pb, 8, 0));
    out(pb); out("\n");
    line("pipe empty", sys(63, pfd[0], (u64)pb, 8, 0));
    sys(57, pfd[0], 0, 0, 0);
    line("pipe widowed", sys(64, pfd[1], (u64)"x", 1, 0));
    // dup3 + fcntl(F_DUPFD) onto a pipe, statx, chdir, rename of a missing file
    int p2[2];
    sys(59, (u64)p2, 0, 0, 0);
    i64 d = sys(25, p2[1], 0, 20, 0);
    line("F_DUPFD >= 20", d >= 20);
    sys(64, d, (u64)"dup", 3, 0);
    line("dup read", sys(63, p2[0], (u64)pb, 8, 0));
    unsigned char sx[256];
    line("statx", sys6(291, (u64)-100, (u64)"/proc/self/exe", 0, 0x7ff, (u64)sx, 0));
    line("statx size > 0", *(u64 *)(sx + 40) > 0);
    line("chdir", sys(49, (u64)"/", 0, 0, 0));
    line("rename missing", sys6(38, (u64)-100, (u64)"/nonexistent/a", (u64)-100, (u64)"/nonexistent/b", 0, 0));

    line("unknown syscall", sys(9999, 0, 0, 0, 0));
    sys(94, 3, 0, 0, 0);
}
