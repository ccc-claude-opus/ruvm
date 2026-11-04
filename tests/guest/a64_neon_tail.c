
int main(void) {
    init_tables();
    u64 all = 0;
#ifdef DBG
    dbg = 1; setrm(0); tests[DBG].fn(); return 0;
#endif
    for (unsigned k = 0; k < sizeof tests / sizeof tests[0]; k++) {
        h = 0xcbf29ce484222325UL;
        asm volatile("msr fpsr, xzr");
        tests[k].fn();
        puts(tests[k].name); puts(" "); puthex(h); putc('\n');
        all ^= h * (k + 1);
    }
    puts("all "); puthex(all); putc('\n');
    return 0;
}
