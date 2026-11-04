// Freestanding self-test: runs identically on host and inside ruvm.
// ruvm output must match the native build byte-for-byte.
typedef unsigned long u64; typedef long i64;
typedef unsigned u32; typedef int i32;
typedef unsigned short u16; typedef short i16;
typedef unsigned char u8; typedef signed char i8;

void out_char(char c);
void *memcpy(void *d, const void *s, unsigned long n);
void *memset(void *d, int c, unsigned long n);

#define NOLOOPOPT __attribute__((optimize("no-tree-loop-distribute-patterns"), noinline))
NOLOOPOPT void *memcpy(void *d, const void *s, unsigned long n) { u8 *a = d; const u8 *b = s; while (n--) *a++ = *b++; return d; }
NOLOOPOPT void *memset(void *d, int c, unsigned long n) { u8 *a = d; while (n--) *a++ = (u8)c; return d; }

static void puts_(const char *s) { while (*s) out_char(*s++); }
static void put_u(u64 v) { char b[24]; int n = 0; if (!v) { out_char('0'); return; } while (v) { b[n++] = '0' + v % 10; v /= 10; } while (n) out_char(b[--n]); }
static void put_i(i64 v) { if (v < 0) { out_char('-'); put_u(0 - (u64)v); } else put_u(v); }
static void put_h(u64 v) { for (int s = 60; s >= 0; s -= 4) out_char("0123456789abcdef"[(v >> s) & 15]); }
static void line(const char *name, u64 v) { puts_(name); puts_(": "); put_h(v); out_char('\n'); }

static u64 popc64(u64 x) { u64 n = 0; while (x) { n += x & 1; x >>= 1; } return n; }
#ifdef __riscv
static u64 clz64(u64 x) { u64 n = 0; while (!(x >> 63)) { x <<= 1; n++; } return n; }
static u64 bswap64(u64 x) { u64 r = 0; for (int i = 0; i < 8; i++) { r = (r << 8) | (x & 0xff); x >>= 8; } return r; }
static u32 bswap32(u32 x) { return (x >> 24) | ((x >> 8) & 0xff00) | ((x << 8) & 0xff0000) | (x << 24); }
#else
#define clz64 __builtin_clzll
#define bswap64 __builtin_bswap64
#define bswap32 __builtin_bswap32
#endif

static u64 fib_rec(u32 n) { return n < 2 ? n : fib_rec(n - 1) + fib_rec(n - 2); }
static u64 fib_it(u32 n) { u64 a = 0, b = 1; while (n--) { u64 t = a + b; a = b; b = t; } return a; }

static u8 sieve[4000];
static u32 primes(void) { memset(sieve, 1, sizeof sieve); u32 c = 0; for (u32 i = 2; i < 4000; i++) if (sieve[i]) { c++; for (u32 j = i * 2; j < 4000; j += i) sieve[j] = 0; } return c; }

static u32 lcg_state = 12345;
static u32 lcg(void) { lcg_state = lcg_state * 1103515245u + 12345u; return lcg_state >> 8; }

static i32 arr[96];
static u64 sort_check(void) {
    for (int i = 0; i < 96; i++) arr[i] = (i32)lcg() - 4000000;
    for (int i = 0; i < 96; i++) for (int j = 0; j + 1 < 96 - i; j++) if (arr[j] > arr[j + 1]) { i32 t = arr[j]; arr[j] = arr[j + 1]; arr[j + 1] = t; }
    u64 h = 1469598103934665603ull;
    for (int i = 0; i < 96; i++) { h ^= (u32)arr[i]; h *= 1099511628211ull; }
    return h;
}

struct S { i8 a; u8 b; i16 c; u16 d; i32 e; u32 f; i64 g; };
static struct S gs = { -5, 250, -30000, 60000, -2000000000, 4000000000u, -9000000000000000000l };
static u64 struct_test(void) {
    struct S l; memcpy(&l, &gs, sizeof l);
    return (u64)(i64)l.a + l.b * 3 + (u64)(i64)l.c * 7 + l.d * 11 + (u64)(i64)l.e * 13 + l.f * 17 + (u64)l.g;
}

static int sw(int x) { switch (x) { case 0: return 11; case 1: return 22; case 2: return 33; case 3: return 44; case 4: return 55; case 5: return 66; case 7: return 88; default: return -1; } }
static int apply(int (*f)(int), int x) { return f(x) * 2; }

__attribute__((noinline)) static u64 arith(u64 a, u64 b) {
    i64 sa = (i64)a, sb = (i64)b; u32 wa = (u32)a, wb = (u32)b; i32 swa = (i32)a, swb = (i32)b;
    u64 h = 0;
    h = h * 31 + a + b; h = h * 31 + (a - b); h = h * 31 + a * b;
    if (b) { h = h * 31 + a / b; h = h * 31 + a % b; }
    if (sb && !(sa == (i64)0x8000000000000000ull && sb == -1)) { h = h * 31 + (u64)(sa / sb); h = h * 31 + (u64)(sa % sb); }
    if (wb) { h = h * 31 + wa / wb; h = h * 31 + wa % wb; }
    if (swb && !(swa == -2147483647 - 1 && swb == -1)) { h = h * 31 + (u64)(i64)(swa / swb); h = h * 31 + (u64)(i64)(swa % swb); }
    h = h * 31 + (a ^ b) + (a | b) + (a & b) + (~a);
    h = h * 31 + (a << (b & 63)) + (a >> (b & 63)) + (u64)(sa >> (b & 63));
    h = h * 31 + (u32)(wa << (wb & 31)) + (wa >> (wb & 31)) + (u32)(swa >> (wb & 31));
    h = h * 31 + (a << 7 | a >> 57) + (wa << 3 | wa >> 29);
    h = h * 31 + (u64)(((unsigned __int128)a * b) >> 64) + (u64)(((__int128)sa * sb) >> 64);
    h = h * 31 + (a < b) + (sa < sb) * 2 + (a >= b) * 4 + (sa >= sb) * 8 + (wa < wb) * 16 + (swa < swb) * 32;
    h = h * 31 + (u64)clz64(a | 1) + (u64)popc64(a);
    h = h * 31 + bswap64(a) + bswap32(wa);
    h = h * 31 + (u64)(i64)(i8)a + (u64)(i64)(i16)a + (u64)(i64)(i32)a + (u8)a + (u16)a + (u32)a;
    h = h * 31 + (a ? a : b) + (sa > 0 ? 1 : -1) + (b == 0 ? 5 : 9);
    return h;
}

static int ack_stack_depth(int n) { volatile char pad[64]; pad[0] = (char)n; return n == 0 ? pad[0] : 1 + ack_stack_depth(n - 1); }

int main(void) {
    line("fib_rec(20)", fib_rec(20));
    line("fib_it(90)", fib_it(90));
    line("primes<4000", primes());
    line("sort_hash", sort_check());
    line("struct", struct_test());
    u64 s = 0; for (int i = -2; i < 10; i++) s = s * 131 + (u64)(i64)sw(i); line("switch", s);
    line("fnptr", (u64)(i64)apply(sw, 3));
    line("stack", (u64)ack_stack_depth(500));
    static const u64 v[] = { 0, 1, 2, 3, 7, 0xff, 0x8000, 0x7fffffff, 0x80000000ull, 0xffffffffull, 0x123456789abcdefull,
        0x7fffffffffffffffull, 0x8000000000000000ull, 0xfedcba9876543210ull, 0xffffffffffffffffull, 1000000007, 1000000009ull * 1000000007ull };
    u64 acc = 0;
    for (unsigned i = 0; i < sizeof v / sizeof v[0]; i++) for (unsigned j = 0; j < sizeof v / sizeof v[0]; j++) { acc = acc * 1000003 + arith(v[i], v[j]); }
    line("arith", acc);
    puts_("done\n");
    return 0;
}
