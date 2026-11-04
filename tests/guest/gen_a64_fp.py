#!/usr/bin/env python3
"""Generate a64_fp.c: every scalar AArch64 FP instruction over special operands, in all
four FPCR rounding modes, hashed (result bits + FPSR).  Output must equal
qemu-system-aarch64 -M virt -cpu max."""
fns = []

def fn(name, body):
    fns.append((name, body))

def loop2(kind, op, ty):
    t, w = ("D", 64) if ty == "d" else ("S", 32)
    r = "u64" if ty == "d" else "u32"
    x = "%1" if ty == "d" else "%w1"; y = "%2" if ty == "d" else "%w2"; o = "%0" if ty == "d" else "%w0"
    return f"""RMS for (unsigned i = 0; i < N{t}; i++) for (unsigned j = 0; j < N{t}; j++) {{ {r} r;
 asm volatile("fmov {ty}1, {x}\\n fmov {ty}2, {y}\\n {op} {ty}0, {ty}1, {ty}2\\n fmov {o}, {ty}0" : "=r"(r) : "r"({t}[i]), "r"({t}[j]) : "v0","v1","v2");
 mix(r); mix(getfpsr()); }} ENDRMS"""

for op in "fadd fsub fmul fdiv fmax fmin fmaxnm fminnm fnmul".split():
    for ty in "ds":
        fn(f"{ty}_{op}", loop2(2, op, ty))

def unary(op, ty):
    t = "D" if ty == "d" else "S"; r = "u64" if ty == "d" else "u32"
    x = "%1" if ty == "d" else "%w1"; o = "%0" if ty == "d" else "%w0"
    return f"""RMS for (unsigned i = 0; i < N{t}; i++) {{ {r} r;
 asm volatile("fmov {ty}1, {x}\\n {op} {ty}0, {ty}1\\n fmov {o}, {ty}0" : "=r"(r) : "r"({t}[i]) : "v0","v1");
 mix(r); mix(getfpsr()); }} ENDRMS"""

for op in "fabs fneg fsqrt frintn frintp frintm frintz frinta frintx frinti".split():
    for ty in "ds":
        fn(f"{ty}_{op}", unary(op, ty))

def fma(op, ty):
    t = "D" if ty == "d" else "S"; r = "u64" if ty == "d" else "u32"
    a = ["%1", "%2", "%3"] if ty == "d" else ["%w1", "%w2", "%w3"]; o = "%0" if ty == "d" else "%w0"
    return f"""RMS for (unsigned i = 0; i < N{t}; i++) for (unsigned j = 0; j < N{t}; j += 2) for (unsigned k = 0; k < N{t}; k += 4) {{ {r} r;
 asm volatile("fmov {ty}1, {a[0]}\\n fmov {ty}2, {a[1]}\\n fmov {ty}3, {a[2]}\\n {op} {ty}0, {ty}1, {ty}2, {ty}3\\n fmov {o}, {ty}0" : "=r"(r) : "r"({t}[i]), "r"({t}[j]), "r"({t}[k]) : "v0","v1","v2","v3");
 mix(r); mix(getfpsr()); }} ENDRMS"""

for op in "fmadd fmsub fnmadd fnmsub".split():
    for ty in "ds":
        fn(f"{ty}_{op}", fma(op, ty))

def cmp(op, ty, zero):
    t = "D" if ty == "d" else "S"
    x = "%0" if ty == "d" else "%w0"; y = "%1" if ty == "d" else "%w1"
    second = "#0.0" if zero else f"{ty}2"
    return f"""for (unsigned i = 0; i < N{t}; i++) for (unsigned j = 0; j < N{t}; j++) {{ u64 f;
 asm volatile("fmov {ty}1, {x}\\n fmov {ty}2, {y}\\n {op} {ty}1, {second}\\n mrs %2, nzcv" :: "r"({t}[i]), "r"({t}[j]), "r"(0UL) : "v1","v2","cc");
 mix(getnzcv()); mix(getfpsr()); }}""".replace('"r"(0UL)', '"r"(0UL)')

for ty in "ds":
    t = "D" if ty == "d" else "S"
    x = "%w1" if ty == "s" else "%1"; y = "%w2" if ty == "s" else "%2"
    for op in ("fcmp", "fcmpe"):
        for zero in (False, True):
            second = "#0.0" if zero else f"{ty}2"
            fn(f"{ty}_{op}{'z' if zero else ''}", f"""for (unsigned i = 0; i < N{t}; i++) for (unsigned j = 0; j < N{t}; j++) {{ u64 f;
 asm volatile("fmov {ty}1, {x}\\n fmov {ty}2, {y}\\n {op} {ty}1, {second}\\n mrs %0, nzcv" : "=r"(f) : "r"({t}[i]), "r"({t}[j]) : "v1","v2","cc");
 mix(f); mix(getfpsr()); }}""")
    for op in ("fccmp", "fccmpe"):
        for cond in ("eq", "lt", "vs"):
            fn(f"{ty}_{op}_{cond}", f"""for (unsigned i = 0; i < N{t}; i++) for (unsigned j = 0; j < N{t}; j++) for (int c = 0; c < 2; c++) {{ u64 f;
 asm volatile("fmov {ty}1, {x}\\n fmov {ty}2, {y}\\n cmp %3, #0\\n {op} {ty}1, {ty}2, #5, {cond}\\n mrs %0, nzcv" : "=r"(f) : "r"({t}[i]), "r"({t}[j]), "r"((u64)c) : "v1","v2","cc");
 mix(f); mix(getfpsr()); }}""")
    fn(f"{ty}_fcsel", f"""for (unsigned i = 0; i < N{t}; i++) for (unsigned j = 0; j < N{t}; j++) for (int c = 0; c < 2; c++) {{ u64 r;
 asm volatile("fmov {ty}1, {x}\\n fmov {ty}2, {y}\\n cmp %3, #0\\n fcsel {ty}0, {ty}1, {ty}2, eq\\n fmov {"%0" if ty=="d" else "%w0"}, {ty}0" : "=r"(r) : "r"({t}[i]), "r"({t}[j]), "r"((u64)c) : "v0","v1","v2","cc");
 mix(r & ({'~0UL' if ty=='d' else '0xffffffffUL'})); }}""")

# FP -> int
for ty in "ds":
    t = "D" if ty == "d" else "S"; x = "%1" if ty == "d" else "%w1"
    for m in "zs zu ns nu ps pu ms mu as au".split():
        for gw in "xw":
            reg = f"{gw}0" if gw == "x" else "w0"
            o = "%0" if gw == "x" else "%w0"
            fn(f"{ty}_fcvt{m}_{gw}", f"""RMS for (unsigned i = 0; i < N{t}; i++) {{ u64 r;
 asm volatile("fmov {ty}1, {x}\\n fcvt{m} {o}, {ty}1" : "=r"(r) : "r"({t}[i]) : "v1");
 mix(r); mix(getfpsr()); }} ENDRMS""")
    for gw in "xw":
        o = "%0" if gw == "x" else "%w0"
        for m in "zs zu".split():
            for fb in (1, 7, 31) if gw == "w" else (1, 7, 63):
                fn(f"{ty}_fcvt{m}_{gw}_fix{fb}", f"""RMS for (unsigned i = 0; i < N{t}; i++) {{ u64 r;
 asm volatile("fmov {ty}1, {x}\\n fcvt{m} {o}, {ty}1, #{fb}" : "=r"(r) : "r"({t}[i]) : "v1");
 mix(r); mix(getfpsr()); }} ENDRMS""")

# int -> FP
INTS = "0UL, 1UL, 0xffffffffffffffffUL, 0x7fffffffffffffffUL, 0x8000000000000000UL, 0x0000000080000000UL, 0xffffffff80000000UL, 0x00ffffffffffffffUL, 0x0123456789abcdefUL, 0x20000000000001UL, 0x1000001UL, 0x000000007fffffffUL"
for ty in "ds":
    o = "%0" if ty == "d" else "%w0"; rt = "u64" if ty == "d" else "u32"
    for m in ("scvtf", "ucvtf"):
        for gw in "xw":
            xi = "%1" if gw == "x" else "%w1"
            fn(f"{m}_{ty}_{gw}", f"""static const u64 I[] = {{ {INTS} }};
 RMS for (unsigned i = 0; i < sizeof I / 8; i++) {{ {rt} r;
 asm volatile("{m} {ty}0, {xi}\\n fmov {o}, {ty}0" : "=r"(r) : "r"(I[i]) : "v0");
 mix(r); mix(getfpsr()); }} ENDRMS""")
            for fb in (1, 9, 31) if gw == "w" else (1, 9, 40, 63):
                fn(f"{m}_{ty}_{gw}_fix{fb}", f"""static const u64 I[] = {{ {INTS} }};
 RMS for (unsigned i = 0; i < sizeof I / 8; i++) {{ {rt} r;
 asm volatile("{m} {ty}0, {xi}, #{fb}\\n fmov {o}, {ty}0" : "=r"(r) : "r"(I[i]) : "v0");
 mix(r); mix(getfpsr()); }} ENDRMS""")

# precision conversions
def conv(dst, src):
    tbl = {"d": ("D", "u64", "%1", "%0"), "s": ("S", "u32", "%w1", "%w0"), "h": ("H", "u32", "%w1", "%w0")}[src]
    out = {"d": ("u64", "%0"), "s": ("u32", "%w0"), "h": ("u32", "%w0")}[dst]
    fn(f"fcvt_{dst}_{src}", f"""RMS for (unsigned i = 0; i < N{tbl[0]}; i++) {{ {out[0]} r;
 asm volatile("fmov {src}1, {tbl[2]}\\n fcvt {dst}0, {src}1\\n fmov {out[1]}, {dst}0" : "=r"(r) : "r"({tbl[0]}[i]) : "v0","v1");
 mix(r & ({'~0UL' if dst=='d' else '0xffffUL' if dst=='h' else '0xffffffffUL'})); mix(getfpsr()); }} ENDRMS""")
for d, s in (("s", "d"), ("d", "s"), ("h", "s"), ("h", "d"), ("s", "h"), ("d", "h")):
    conv(d, s)

# fmov
for imm in ("1.0", "-2.5", "0.125", "31.0", "-0.5", "1.9375", "10.0"):
    fn(f"fmov_imm_{imm.replace('.', '_').replace('-', 'm')}", f"""{{ u64 a, b;
 asm volatile("fmov d0, #{imm}\\n fmov %0, d0\\n fmov s1, #{imm}\\n fmov %w1, s1" : "=r"(a), "=r"(b) :: "v0","v1"); mix(a); mix(b); }}""")

fn("fmov_gp", """for (unsigned i = 0; i < ND; i++) { u64 a, b, c; unsigned __int128 q;
 asm volatile("fmov d0, %3\\n fmov %0, d0\\n fmov v1.d[1], %3\\n fmov %1, v1.d[1]\\n fmov s2, %w3\\n fmov %w2, s2" : "=r"(a), "=r"(b), "=r"(c) : "r"(D[i]) : "v0","v1","v2");
 mix(a); mix(b); mix(c); }""")

out = ["// GENERATED by gen_a64_fp.py -- do not edit.", open("a64_fp_head.c").read()]
for name, body in fns:
    out.append(f"static void t_{name}(void) {{\n{body}\n}}")
out.append("struct t { const char *name; void (*fn)(void); };\nstatic const struct t tests[] = {")
for name, _ in fns:
    out.append(f'    {{"{name}", t_{name}}},')
out.append("};")
out.append(open("a64_fp_tail.c").read())
open("a64_fp.c", "w").write("\n".join(out))
print(len(fns), "tests")
