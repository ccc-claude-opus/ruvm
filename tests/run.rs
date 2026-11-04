//! Hand-assembled programs run on the interpreters, plus the guest ELF suites.

use ruvm::demo::{a64, rv};
use ruvm::{Arch, Machine, Stop};
use std::process::Command;

fn run(arch: Arch, code: &[u32], limit: u64) -> Stop {
    let mut m = Machine::new(arch, 4, false);
    m.load_raw(arch.ram_base(), &rv::bytes(code)).unwrap();
    m.run(Some(limit))
}

#[test]
fn riscv_sum_loop_exits_via_ecall() {
    let code = [
        rv::addi(10, 0, 0),    // a0 = 0
        rv::addi(11, 0, 100),  // a1 = 100
        rv::add(10, 10, 11),   // loop: a0 += a1
        rv::addi(11, 11, -1),  // a1--
        rv::bne(11, 0, -8),    // while a1 != 0
        rv::addi(17, 0, 93),   // a7 = exit
        rv::ecall(),
    ];
    assert_eq!(run(Arch::Riscv64, &code, 10_000), Stop::Halt(5050));
}

#[test]
fn riscv_trap_handler_and_mret() {
    let code = [
        rv::auipc(5, 0),             //  0: t0 = pc
        rv::addi(5, 5, 48),          //  1: t0 -> handler
        rv::csrrw(0, 0x305, 5),      //  2: mtvec = t0
        rv::ecall(),                 //  3: trap -> handler (mcause = 11)
        rv::csrrs(10, 0x340, 0),     //  4: a0 = mscratch (set by handler)
        rv::slli(10, 10, 16),        //  5
        rv::lui(7, 3),               //  6
        rv::addi(7, 7, 0x333),       //  7: t2 = 0x3333
        rv::or(7, 7, 10),            //  8: t2 = (mcause << 16) | 0x3333  (FAIL code)
        rv::lui(6, 0x100),           //  9: t1 = test finisher
        rv::sw(6, 7, 0),             // 10
        rv::jal(0, 0),               // 11
        rv::csrrs(28, 0x342, 0),     // 12: handler: t3 = mcause
        rv::csrrw(0, 0x340, 28),     // 13: mscratch = t3
        rv::csrrs(29, 0x341, 0),     // 14: t4 = mepc
        rv::addi(29, 29, 4),         // 15: skip the ecall
        rv::csrrw(0, 0x341, 29),     // 16: mepc = t4
        rv::mret(),                  // 17
    ];
    assert_eq!(run(Arch::Riscv64, &code, 1000), Stop::Halt(11));
}

#[test]
fn riscv_unhandled_faults_are_fatal() {
    // illegal instruction (all zeros)
    assert!(matches!(run(Arch::Riscv64, &[0], 10), Stop::Fatal(m) if m.contains("cause=2")));
    // load from unmapped address 0
    assert!(matches!(run(Arch::Riscv64, &[rv::ld(10, 0, 0)], 10), Stop::Fatal(m) if m.contains("cause=5")));
}

#[test]
fn aarch64_sum_loop_exits_via_svc() {
    let code = [
        a64::movz(0, 0, 0),       // x0 = 0
        a64::movz(1, 100, 0),     // x1 = 100
        a64::add_reg(0, 0, 1),    // loop: x0 += x1
        a64::subs_imm(1, 1, 1),   // x1 -= 1 (sets flags)
        a64::b_cond(1, -2),       // b.ne loop
        a64::movz(8, 93, 0),      // x8 = exit
        a64::svc(0),
    ];
    assert_eq!(run(Arch::Aarch64, &code, 10_000), Stop::Halt(5050));
}

#[test]
fn aarch64_psci_and_undefined() {
    let off = [a64::movz(0, 8, 0), a64::movk(0, 0x8400, 1), a64::hvc(0)];
    assert_eq!(run(Arch::Aarch64, &off, 100), Stop::Halt(0));
    assert!(matches!(run(Arch::Aarch64, &[0], 10), Stop::Fatal(m) if m.contains("undefined")));
}

#[test]
fn instruction_limit() {
    assert_eq!(run(Arch::Riscv64, &[rv::jal(0, 0)], 1000), Stop::Limit);
    assert_eq!(run(Arch::Aarch64, &[a64::b(0)], 1000), Stop::Limit);
}

// ---------------------------------------------------------------- guest ELFs

fn fnv1a(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

fn run_elf(arch: &str, name: &str) -> Vec<u8> {
    let path = format!("{}/tests/guest/prebuilt/{}", env!("CARGO_MANIFEST_DIR"), name);
    let out = Command::new(env!("CARGO_BIN_EXE_ruvm")).args(["-M", arch, "-kernel", &path]).output().unwrap();
    assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

fn expected(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/guest/prebuilt/{}", env!("CARGO_MANIFEST_DIR"), name)).unwrap()
}

/// Output of a gcc-compiled C program must equal the natively compiled build's output.
#[test]
fn gcc_compiled_selftest_matches_native() {
    let want = expected("selftest.expected");
    assert_eq!(String::from_utf8(run_elf("riscv64", "selftest_rv.elf")).unwrap(), want);
    assert_eq!(String::from_utf8(run_elf("aarch64", "selftest_a64.elf")).unwrap(), want);
}

/// Instruction-by-instruction dumps must equal what qemu-user produced (hash recorded
/// by tests/guest/build.sh).
#[test]
fn alu_dump_matches_qemu() {
    for (arch, elf, exp) in [("riscv64", "rv_alu.elf", "rv_alu.expected"), ("aarch64", "a64_alu.elf", "a64_alu.expected")] {
        let out = run_elf(arch, elf);
        let got = format!("{} {:016x}\n", out.len(), fnv1a(&out));
        assert_eq!(got, expected(exp), "{arch} output differs from qemu reference");
    }
}
