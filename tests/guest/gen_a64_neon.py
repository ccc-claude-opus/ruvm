#!/usr/bin/env python3
"""Generate a64_neon.c: Advanced SIMD instructions x arrangements x edge-case operands,
hashed (v0 result + FPSR); output must equal qemu-system-aarch64 -M virt -cpu max."""
import sys
tests = []   # (name, kind, rms, nops, insn)

import re
def T(name, insn, kind="int", rms=False, nops=2):
    name = re.sub(r"[^A-Za-z0-9_]", "_", name)
    if any(t[0] == name for t in tests):
        name += "_x%d" % len(tests)
    tests.append((name, kind, rms, nops, insn))

BHS = ["8b", "16b", "4h", "8h", "2s", "4s"]
ALL = BHS + ["2d"]

def arr_ok(m, a):
    return True

# ---------------------------------------------------------------- integer three-same
for m in "add sub cmeq cmgt cmge cmhi cmhs cmtst sshl ushl srshl urshl sqshl uqshl sqrshl uqrshl sqadd uqadd sqsub uqsub".split():
    for a in ALL:
        T(f"{m}_{a}", f"{m} v0.{a}, v1.{a}, v2.{a}")
for m in "mul mla mls smax smin umax umin sabd uabd saba uaba shadd uhadd srhadd urhadd shsub uhsub sqdmulh sqrdmulh smaxp sminp umaxp uminp".split():
    for a in BHS:
        if m in ("sqdmulh", "sqrdmulh") and a[-1] == "b": continue
        if m in ("mul", "mla", "mls") and a[-1] == "b" and False: continue
        if a.endswith("b") and m in ("sqdmulh","sqrdmulh"): continue
        T(f"{m}_{a}", f"{m} v0.{a}, v1.{a}, v2.{a}")
for a in ALL:
    T(f"addp_{a}", f"addp v0.{a}, v1.{a}, v2.{a}")
for m in "and orr eor bic orn bsl bit bif".split():
    for a in ("8b", "16b"):
        T(f"{m}_{a}", f"{m} v0.{a}, v1.{a}, v2.{a}")
for a in ("8b", "16b"):
    T(f"pmul_{a}", f"pmul v0.{a}, v1.{a}, v2.{a}")

# ---------------------------------------------------------------- three different
LONG = {"8h": ("8b", "16b"), "4s": ("4h", "8h"), "2d": ("2s", "4s")}
for m in "saddl uaddl ssubl usubl sabal uabal sabdl uabdl smlal umlal smlsl umlsl smull umull".split():
    for d, (n1, n2) in LONG.items():
        T(f"{m}_{d}", f"{m} v0.{d}, v1.{n1}, v2.{n1}")
        T(f"{m}2_{d}", f"{m}2 v0.{d}, v1.{n2}, v2.{n2}")
for m in "saddw uaddw ssubw usubw".split():
    for d, (n1, n2) in LONG.items():
        T(f"{m}_{d}", f"{m} v0.{d}, v1.{d}, v2.{n1}")
        T(f"{m}2_{d}", f"{m}2 v0.{d}, v1.{d}, v2.{n2}")
for m in "sqdmlal sqdmlsl sqdmull".split():
    for d, (n1, n2) in list(LONG.items())[1:]:
        T(f"{m}_{d}", f"{m} v0.{d}, v1.{n1}, v2.{n1}")
        T(f"{m}2_{d}", f"{m}2 v0.{d}, v1.{n2}, v2.{n2}")
for m in "addhn raddhn subhn rsubhn".split():
    for d, (n1, n2) in LONG.items():
        T(f"{m}_{d}", f"{m} v0.{n1}, v1.{d}, v2.{d}")
        T(f"{m}2_{d}", f"{m}2 v0.{n2}, v1.{d}, v2.{d}")
T("pmull_8h", "pmull v0.8h, v1.8b, v2.8b"); T("pmull2_8h", "pmull2 v0.8h, v1.16b, v2.16b")
T("pmull_1q", "pmull v0.1q, v1.1d, v2.1d"); T("pmull2_1q", "pmull2 v0.1q, v1.2d, v2.2d")

# ---------------------------------------------------------------- two-register misc
for m in "rev64 cls clz abs neg sqabs sqneg cmgt0 cmge0 cmeq0 cmle0 cmlt0 suqadd usqadd".split():
    for a in ALL:
        if m in ("cls", "clz", "rev64") and a == "2d": continue
        if m.endswith("0"):
            T(f"{m}_{a}", f"{m[:-1]} v0.{a}, v1.{a}, #0")
        else:
            T(f"{m}_{a}", f"{m} v0.{a}, v1.{a}")
for a in ("8b", "16b", "4h", "8h"):
    T(f"rev32_{a}" if a[-1] != "h" or True else "", f"rev32 v0.{a}, v1.{a}") if a[-1] == "b" or a[-1] == "h" else None
for a in ("8b", "16b"):
    T(f"rev16_{a}", f"rev16 v0.{a}, v1.{a}"); T(f"cnt_{a}", f"cnt v0.{a}, v1.{a}"); T(f"not_{a}", f"not v0.{a}, v1.{a}"); T(f"rbit_{a}", f"rbit v0.{a}, v1.{a}")
for m in "saddlp uaddlp sadalp uadalp".split():
    for a in ("8b", "16b", "4h", "8h", "2s", "4s"):
        d = {"8b": "4h", "16b": "8h", "4h": "2s", "8h": "4s", "2s": "1d", "4s": "2d"}[a]
        T(f"{m}_{a}", f"{m} v0.{d}, v1.{a}")
for m in "xtn sqxtn uqxtn sqxtun".split():
    for d, s in (("8b", "8h"), ("4h", "4s"), ("2s", "2d")):
        T(f"{m}_{d}", f"{m} v0.{d}, v1.{s}")
        d2 = {"8b": "16b", "4h": "8h", "2s": "4s"}[d]
        T(f"{m}2_{d}", f"{m}2 v0.{d2}, v1.{s}")
for d, s, sh in (("8h", "8b", 8), ("4s", "4h", 16), ("2d", "2s", 32)):
    T(f"shll_{d}", f"shll v0.{d}, v1.{s}, #{sh}")
    T(f"shll2_{d}", f"shll2 v0.{d}, v1.{ {'8b':'16b','4h':'8h','2s':'4s'}[s] }, #{sh}")

# ---------------------------------------------------------------- across lanes
for m in "addv smaxv sminv umaxv uminv".split():
    for a, r in (("8b", "b"), ("16b", "b"), ("4h", "h"), ("8h", "h"), ("4s", "s")):
        T(f"{m}_{a}", f"{m} {r}0, v1.{a}")
for m in "saddlv uaddlv".split():
    for a, r in (("8b", "h"), ("16b", "h"), ("4h", "s"), ("8h", "s"), ("4s", "d")):
        T(f"{m}_{a}", f"{m} {r}0, v1.{a}")
T("addp_d", "addp d0, v1.2d")

# ---------------------------------------------------------------- copy
for a, esz, n in (("b", 1, 16), ("h", 2, 8), ("s", 4, 4), ("d", 8, 2)):
    for idx in sorted({0, 1, n - 1}):
        for arr in {"b": ("8b", "16b"), "h": ("4h", "8h"), "s": ("2s", "4s"), "d": ("2d",)}[a]:
            T(f"dup_{arr}_e{idx}", f"dup v0.{arr}, v1.{a}[{idx}]")
        T(f"ins_{a}{idx}", f"ins v0.{a}[{idx}], v1.{a}[{(idx + 1) % n}]")
        T(f"insg_{a}{idx}", f"mov x5, x6\n ins v0.{a}[{idx}], {'w5' if a != 'd' else 'x5'}")
        T(f"umov_{a}{idx}", f"umov {'w5' if a != 'd' else 'x5'}, v1.{a}[{idx}]\n fmov d0, x5")
        if a != "d":
            if a != "s": T(f"smovw_{a}{idx}", f"smov w5, v1.{a}[{idx}]\n fmov d0, x5")
            if a != "s": T(f"smovx_{a}{idx}", f"smov x5, v1.{a}[{idx}]\n fmov d0, x5")
            else: T(f"smovx_{a}{idx}", f"smov x5, v1.s[{idx}]\n fmov d0, x5")
for arr in ("8b", "16b", "4h", "8h", "2s", "4s", "2d"):
    T(f"dupg_{arr}", f"mov x5, #0x1234\n movk x5, #0x5678, lsl #16\n movk x5, #0x9abc, lsl #32\n dup v0.{arr}, {'x5' if arr == '2d' else 'w5'}")

# ---------------------------------------------------------------- modified immediate
for imm in (0, 0x55, 0xab, 0xff):
    for a in ("2s", "4s"):
        for sh in (0, 8, 16, 24):
            for m in ("movi", "mvni", "orr", "bic"):
                T(f"{m}_{a}_{imm:x}_{sh}", f"{m} v0.{a}, #{imm}, lsl #{sh}")
        for sh in (8, 16):
            for m in ("movi", "mvni"):
                T(f"{m}_{a}_{imm:x}_msl{sh}", f"{m} v0.{a}, #{imm}, msl #{sh}")
    for a in ("4h", "8h"):
        for sh in (0, 8):
            for m in ("movi", "mvni", "orr", "bic"):
                T(f"{m}_{a}_{imm:x}_{sh}", f"{m} v0.{a}, #{imm}, lsl #{sh}")
    for a in ("8b", "16b"):
        T(f"movi_{a}_{imm:x}", f"movi v0.{a}, #{imm}")
for imm in ("0x00", "0xff", "0xa5", "0x0f"):
    T(f"movi_2d_{imm}", f"movi v0.2d, #{ '0x' + ''.join('ff' if int(imm,16)>>k&1 else '00' for k in range(7,-1,-1)) }")
    T(f"movi_d_{imm}", f"movi d0, #{ '0x' + ''.join('ff' if int(imm,16)>>k&1 else '00' for k in range(7,-1,-1)) }")
for v in ("1.0", "-2.5", "0.125", "31.0"):
    T(f"fmov_2s_{v}", f"fmov v0.2s, #{v}"); T(f"fmov_4s_{v}", f"fmov v0.4s, #{v}"); T(f"fmov_2d_{v}", f"fmov v0.2d, #{v}")

# ---------------------------------------------------------------- shifts by immediate
for m in "sshr ushr ssra usra srshr urshr srsra ursra sri".split():
    for a in ALL:
        bits = {"b": 8, "h": 16, "s": 32, "d": 64}[a[-1]]
        for sh in sorted({1, bits // 2, bits}):
            T(f"{m}_{a}_{sh}", f"{m} v0.{a}, v1.{a}, #{sh}")
for m in "shl sli sqshl uqshl sqshlu".split():
    for a in ALL:
        bits = {"b": 8, "h": 16, "s": 32, "d": 64}[a[-1]]
        for sh in sorted({0, 1, bits // 2, bits - 1}):
            T(f"{m}_{a}_{sh}", f"{m} v0.{a}, v1.{a}, #{sh}")
NARROW = (("8b", "8h", 8), ("4h", "4s", 16), ("2s", "2d", 32))
for m in "shrn rshrn sqshrn uqshrn sqrshrn uqrshrn sqshrun sqrshrun".split():
    for d, s, bits in NARROW:
        d2 = {"8b": "16b", "4h": "8h", "2s": "4s"}[d]
        for sh in sorted({1, bits // 2, bits}):
            T(f"{m}_{d}_{sh}", f"{m} v0.{d}, v1.{s}, #{sh}")
            T(f"{m}2_{d}_{sh}", f"{m}2 v0.{d2}, v1.{s}, #{sh}")
for m in "sshll ushll".split():
    for s, d, bits in (("8b", "8h", 8), ("4h", "4s", 16), ("2s", "2d", 32)):
        s2 = {"8b": "16b", "4h": "8h", "2s": "4s"}[s]
        for sh in sorted({0, 1, bits - 1}):
            T(f"{m}_{d}_{sh}", f"{m} v0.{d}, v1.{s}, #{sh}")
            T(f"{m}2_{d}_{sh}", f"{m}2 v0.{d}, v1.{s2}, #{sh}")

# ---------------------------------------------------------------- permute / ext / tbl
for m in "zip1 zip2 uzp1 uzp2 trn1 trn2".split():
    for a in ALL:
        T(f"{m}_{a}", f"{m} v0.{a}, v1.{a}, v2.{a}")
for a in ("8b", "16b"):
    for imm in (0, 1, 5, 7) if a == "8b" else (0, 1, 7, 15):
        T(f"ext_{a}_{imm}", f"ext v0.{a}, v1.{a}, v2.{a}, #{imm}")
for a in ("8b", "16b"):
    for n in range(1, 5):
        regs = ", ".join(f"v{k}.16b" for k in (1, 2, 3, 4)[:n])  # v1.. as table; index vector in v5
        T(f"tbl{n}_{a}", f"mov v5.16b, v2.16b\n tbl v0.{a}, {{{regs}}}, v5.{a}", nops=3)
        T(f"tbx{n}_{a}", f"mov v5.16b, v2.16b\n tbx v0.{a}, {{{regs}}}, v5.{a}", nops=3)

# ---------------------------------------------------------------- by element
for m in "mul mla mls sqdmulh sqrdmulh".split():
    for a, e in (("4h", "h"), ("8h", "h"), ("2s", "s"), ("4s", "s")):
        for idx in ((0, 3, 7) if e == "h" else (0, 1, 3)):
            if e == "s" and idx > 3: continue
            if a in ("2s",) and idx > 3: continue
            T(f"{m}_{a}_{e}{idx}", f"{m} v0.{a}, v1.{a}, v2.{e}[{idx}]")
for m in "smull umull smlal umlal smlsl umlsl sqdmull sqdmlal sqdmlsl".split():
    for d, n1, n2, e in (("4s", "4h", "8h", "h"), ("2d", "2s", "4s", "s")):
        for idx in ((0, 5) if e == "h" else (0, 3)):
            T(f"{m}_{d}_{idx}", f"{m} v0.{d}, v1.{n1}, v2.{e}[{idx}]")
            T(f"{m}2_{d}_{idx}", f"{m}2 v0.{d}, v1.{n2}, v2.{e}[{idx}]")

# ---------------------------------------------------------------- scalar integer
for m in "add sub cmeq cmgt cmge cmhi cmhs cmtst sshl ushl srshl urshl sqshl uqshl sqrshl uqrshl".split():
    T(f"s_{m}_d", f"{m} d0, d1, d2")
for m in "sqadd uqadd sqsub uqsub".split():
    for r in "bhsd":
        T(f"s_{m}_{r}", f"{m} {r}0, {r}1, {r}2")
for m in "sqdmulh sqrdmulh".split():
    for r in "hs": T(f"s_{m}_{r}", f"{m} {r}0, {r}1, {r}2")
for m in "abs neg".split(): T(f"s_{m}", f"{m} d0, d1")
for m in "cmgt cmge cmeq cmle cmlt".split(): T(f"s_{m}0", f"{m} d0, d1, #0")
for m in "sqxtn uqxtn sqxtun".split():
    for d, s in (("b", "h"), ("h", "s"), ("s", "d")): T(f"s_{m}_{d}", f"{m} {d}0, {s}1")
for m in "sshr ushr ssra usra srshr urshr sri shl sli".split():
    for sh in (1, 17, 63) if m != "shl" and m != "sli" else (0, 17, 63):
        T(f"s_{m}_{sh}", f"{m} d0, d1, #{sh}")
for m in "sqshl uqshl sqshlu".split():
    for sh in (0, 3, 31): T(f"s_{m}_s{sh}", f"{m} s0, s1, #{sh}")
T("s_dup_d", "mov d0, v1.d[1]"); T("s_dup_s", "mov s0, v1.s[2]"); T("s_dup_h", "mov h0, v1.h[5]"); T("s_dup_b", "mov b0, v1.b[9]")

# ---------------------------------------------------------------- floating-point vector
F32A = ("2s", "4s"); F64A = ("2d",)
for m in "fadd fsub fmul fdiv fmax fmin fmaxnm fminnm fabd fmulx faddp fmaxp fminp fmaxnmp fminnmp fcmeq fcmge fcmgt facge facgt frecps frsqrts".split():
    for a in F32A + F64A:
        T(f"{m}_{a}", f"{m} v0.{a}, v1.{a}, v2.{a}", kind="f64" if a == "2d" else "f32", rms=True)
for m in "fmla fmls".split():
    for a in F32A + F64A:
        T(f"{m}_{a}", f"{m} v0.{a}, v1.{a}, v2.{a}", kind="f64" if a == "2d" else "f32", rms=True)
for m in "fabs fneg fsqrt frintn frintp frintm frintz frinta frintx frinti fcvtzs fcvtzu fcvtns fcvtnu fcvtps fcvtpu fcvtms fcvtmu fcvtas fcvtau scvtf ucvtf".split():
    for a in F32A + F64A:
        T(f"{m}_{a}", f"{m} v0.{a}, v1.{a}", kind="f64" if a == "2d" else "f32", rms=True)
for m in "fcmeq fcmge fcmgt fcmle fcmlt".split():
    for a in F32A + F64A:
        T(f"{m}0_{a}", f"{m} v0.{a}, v1.{a}, #0.0", kind="f64" if a == "2d" else "f32")
for m in "scvtf ucvtf fcvtzs fcvtzu".split():
    for a in F32A + F64A:
        for fb in (1, 7, 31) if a != "2d" else (1, 7, 63):
            T(f"{m}_fix_{a}_{fb}", f"{m} v0.{a}, v1.{a}, #{fb}", kind="f64" if a == "2d" else "f32", rms=True)
T("fcvtn_2s", "fcvtn v0.2s, v1.2d", kind="f64", rms=True); T("fcvtn2_4s", "fcvtn2 v0.4s, v1.2d", kind="f64", rms=True)
T("fcvtl_2d", "fcvtl v0.2d, v1.2s", kind="f32"); T("fcvtl2_2d", "fcvtl2 v0.2d, v1.4s", kind="f32")
T("fcvtn_4h", "fcvtn v0.4h, v1.4s", kind="f32", rms=True); T("fcvtl_4s", "fcvtl v0.4s, v1.4h", kind="f32")
T("fcvtxn_2s", "fcvtxn v0.2s, v1.2d", kind="f64", rms=True); T("fcvtxn2_4s", "fcvtxn2 v0.4s, v1.2d", kind="f64", rms=True)
for m in "fmul fmla fmls fmulx".split():
    for a, e in (("2s", "s"), ("4s", "s"), ("2d", "d")):
        for idx in ((0, 1, 3) if e == "s" else (0, 1)):
            T(f"{m}_{a}_{idx}", f"{m} v0.{a}, v1.{a}, v2.{e}[{idx}]", kind="f64" if a == "2d" else "f32", rms=True)
for m in "fmaxv fminv fmaxnmv fminnmv".split():
    T(f"{m}_4s", f"{m} s0, v1.4s", kind="f32")
for m in "faddp fmaxp fminp fmaxnmp fminnmp".split():
    T(f"s_{m}_s", f"{m} s0, v1.2s", kind="f32"); T(f"s_{m}_d", f"{m} d0, v1.2d", kind="f64")
for m in "fadd fmul fsub fabd fmulx fcmeq fcmge fcmgt facge facgt frecps frsqrts".split():
    T(f"s_{m}_s", f"{m} s0, s1, s2", kind="f32", rms=True); T(f"s_{m}_d", f"{m} d0, d1, d2", kind="f64", rms=True)
for m in "fcvtzs fcvtzu fcvtns fcvtnu fcvtps fcvtpu fcvtms fcvtmu fcvtas fcvtau scvtf ucvtf".split():
    T(f"s_{m}_s", f"{m} s0, s1", kind="f32", rms=True); T(f"s_{m}_d", f"{m} d0, d1", kind="f64", rms=True)
for m in "fcmeq fcmge fcmgt fcmle fcmlt".split():
    T(f"s_{m}0_s", f"{m} s0, s1, #0.0", kind="f32"); T(f"s_{m}0_d", f"{m} d0, d1, #0.0", kind="f64")
T("s_fmla_s", "fmla s0, s1, v2.s[1]", kind="f32", rms=True); T("s_fmul_d", "fmul d0, d1, v2.d[1]", kind="f64", rms=True)
T("s_fcvtxn", "fcvtxn s0, d1", kind="f64", rms=True)

out = ["// GENERATED by gen_a64_neon.py -- do not edit.", open("a64_neon_head.c").read()]
kinds = {"int": "PI", "f32": "PF", "f64": "PD"}
for name, kind, rms, nops, insn in tests:
    P = kinds[kind]
    insn_c = insn.replace("\n", "\\n")
    rm_open = "RMS " if rms else ""
    rm_close = " ENDRMS" if rms else ""
    out.append(f"""static void t_{name}(void) {{ {rm_open}
 for (unsigned i = 0; i < NP; i++) for (unsigned j = 0; j < NP; j++) {{ u64 out[2]; const u64 *pa = {P}[i], *pb = {P}[j], *pd = {P}[(i * 5 + j * 3 + 1) % NP];
  asm volatile("ldr q1, [%1]\\n ldr q2, [%2]\\n ldr q0, [%3]\\n ldr q3, [%2]\\n ldr q4, [%1]\\n mov x6, %4\\n {insn_c}\\n str q0, [%0]" :: "r"(out), "r"(pa), "r"(pb), "r"(pd), "r"(pa[1]) : "v0","v1","v2","v3","v4","v5","x5","x6","cc","memory");
  rec(i, j, out[0], out[1]); }} {rm_close} }}""")
out.append("struct t { const char *name; void (*fn)(void); };\nstatic const struct t tests[] = {")
for name, *_ in tests:
    out.append(f'    {{"{name}", t_{name}}},')
out.append("};")
out.append(open("a64_neon_tail.c").read())
open("a64_neon.c", "w").write("\n".join(out))
print(len(tests), "tests")
