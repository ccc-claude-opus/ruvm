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

/// The C-extension (rv64imac) build of the same C program.
#[test]
fn compressed_selftest_matches_native() {
    let want = expected("selftest.expected");
    assert_eq!(String::from_utf8(run_elf("riscv64", "selftest_rvc.elf")).unwrap(), want);
}

/// Full-system tests (privilege modes, traps, paging, interrupts): the console output
/// must equal what qemu-system-* printed for the same ELF (tests/guest/build_sys.sh).
#[test]
fn riscv_privileged_matches_qemu_system() {
    let out = String::from_utf8(run_elf("riscv64", "rv_priv.elf")).unwrap();
    assert_eq!(out, expected("rv_priv.expected"));
}

#[test]
fn aarch64_exceptions_match_qemu_system() {
    let out = String::from_utf8(run_elf("aarch64", "a64_priv.elf")).unwrap();
    assert_eq!(out, expected("a64_priv.expected"));
}

#[test]
fn riscv_compressed_hand_assembled() {
    // c.li a0, 7 ; c.addi a0, 3 ; c.li a7, 0 ; c.addi a7, 31 x3 ; ecall  -> exit(10)
    let mut img = Vec::new();
    for h in [0x451du16, 0x050d, 0x4881, 0x08fd, 0x08fd, 0x08fd] {
        img.extend_from_slice(&h.to_le_bytes());
    }
    img.extend_from_slice(&rv::ecall().to_le_bytes());
    let mut m = Machine::new(Arch::Riscv64, 4, false);
    m.load_raw(Arch::Riscv64.ram_base(), &img).unwrap();
    assert_eq!(m.run(Some(100)), Stop::Halt(10));
}

/// Boot MIT xv6-riscv (kernel + file system from tests/guest/build_xv6.sh) on the
/// virtio disk, type two commands into the shell and check their output.
#[test]
fn xv6_boots_and_runs_shell_commands() {
    let dir = format!("{}/tests/guest/prebuilt/xv6", env!("CARGO_MANIFEST_DIR"));
    let out = Command::new(env!("CARGO_BIN_EXE_ruvm"))
        .args(["-M", "riscv64", "-m", "128", "-kernel", &format!("{dir}/kernel")])
        .args(["-drive", &format!("file={dir}/fs.img"), "-snapshot"])
        .args(["-input", "echo hi from ruvm\\nmkdir d\\necho data > d/f\\ncat d/f | wc\\nls d\\n"])
        .args(["-icount", "800000000"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(2), "expected to stop at the instruction limit:\n{text}");
    for want in ["xv6 kernel is booting", "init: starting sh", "$ hi from ruvm", "1 1 5", "f              2 "] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
}

/// Every F/D instruction over special operands in all rounding modes: result bits and
/// fflags must hash-match qemu-system-riscv64.
#[test]
fn riscv_fp_matches_qemu_system() {
    let out = String::from_utf8(run_elf("riscv64", "rv_fp.elf")).unwrap();
    assert_eq!(out, expected("rv_fp.expected"));
}

fn sbi_payload(extra: &[&str]) -> String {
    let dir = format!("{}/tests/guest/prebuilt", env!("CARGO_MANIFEST_DIR"));
    let out = Command::new(env!("CARGO_BIN_EXE_ruvm"))
        .args(["-M", "riscv64", "-kernel", &format!("{dir}/rv_sbi.Image"), "-icount", "200000000"])
        .args(extra.iter().map(|a| a.replace("@DIR@", &dir)))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    text[text.find("S-mode payload").expect("payload did not run")..].to_string()
}

/// An S-mode payload (Linux Image format) using SBI: ruvm's built-in SBI must behave
/// like OpenSBI on QEMU.
#[test]
fn builtin_sbi_matches_opensbi_on_qemu() {
    assert_eq!(sbi_payload(&[]), expected("rv_sbi.expected"));
}

/// Real OpenSBI firmware (QEMU's fw_dynamic) running on ruvm in M-mode.
#[test]
fn opensbi_firmware_boots_payload() {
    assert_eq!(sbi_payload(&["-bios", "@DIR@/opensbi-fw_dynamic.bin"]), expected("rv_sbi.expected"));
}

/// Debian's stock riscv64 kernel + installer initrd (tools/linux/fetch-debian.sh) boots
/// to a busybox shell, runs commands and powers off. Slow (~1 min): run with
/// `cargo test --release -- --ignored`.
#[test]
#[ignore]
fn debian_linux_riscv64_boots_to_shell() {
    let dir = format!("{}/tests/guest/out/linux/debian/riscv64", env!("CARGO_MANIFEST_DIR"));
    if !std::path::Path::new(&format!("{dir}/linux")).exists() {
        eprintln!("skipped: run tools/linux/fetch-debian.sh first");
        return;
    }
    let out = Command::new(env!("CARGO_BIN_EXE_ruvm"))
        .args(["-M", "riscv64", "-m", "512", "-kernel", &format!("{dir}/linux")])
        .args(["-initrd", &format!("{dir}/initrd.gz"), "-append", "console=ttyS0 rdinit=/bin/sh"])
        .args(["-send-after", "# ", "mount -t proc p /proc; uname -sm; grep isa /proc/cpuinfo; echo 6*7=$((6*7))\\n"])
        .args(["-send-after", "=42", "\\npoweroff -f\\n", "-icount", "10000000000"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "did not power off cleanly:\n{text}");
    for want in ["Run /bin/sh as init process", "Linux riscv64", "rv64imafdc", "6*7=42", "reboot: Power down"] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
}

fn same_as_qemu(arch: &str, elf: &str) {
    let out = String::from_utf8(run_elf(arch, elf)).unwrap();
    assert_eq!(out, expected(&elf.replace(".elf", ".expected")), "{elf} differs from the qemu-system reference");
}

/// Every scalar FP instruction (half/single/double) over special operands in all FPCR
/// rounding modes, hashed with the FPSR flags: identical to qemu-system-aarch64.
#[test]
fn aarch64_scalar_fp_matches_qemu_system() {
    same_as_qemu("aarch64", "a64_fp.elf");
}

/// ~2000 Advanced SIMD instruction x arrangement groups (integer, FP, narrowing/widening,
/// shifts, permutes, by-element, scalar forms) over edge-case vectors.
#[test]
fn aarch64_neon_matches_qemu_system() {
    same_as_qemu("aarch64", "a64_neon.elf");
}

/// Stage-1 MMU: translation / access-flag / permission faults per level, execute-never,
/// TTBR1, 2 MiB blocks, page-crossing accesses, TBI, LDTR/STTR, DC ZVA, TLBI, EL0 faults.
#[test]
fn aarch64_mmu_matches_qemu_system() {
    same_as_qemu("aarch64", "a64_mmu.elf");
}

/// GICv2, generic virtual timer, SGI priorities, priority mask, PL011 TX interrupt.
#[test]
fn aarch64_interrupts_match_qemu_system() {
    same_as_qemu("aarch64", "a64_irq.elf");
}

/// Debian's stock arm64 kernel + installer initrd boots to a busybox shell, runs commands
/// and powers off through PSCI. Slow (~2 min): `cargo test --release -- --ignored`.
#[test]
#[ignore]
fn debian_linux_aarch64_boots_to_shell() {
    let dir = format!("{}/tests/guest/out/linux/debian/arm64", env!("CARGO_MANIFEST_DIR"));
    if !std::path::Path::new(&format!("{dir}/linux")).exists() {
        eprintln!("skipped: run tools/linux/fetch-debian.sh arm64 first");
        return;
    }
    let out = Command::new(env!("CARGO_BIN_EXE_ruvm"))
        .args(["-M", "aarch64", "-m", "1024", "-kernel", &format!("{dir}/linux")])
        .args(["-initrd", &format!("{dir}/initrd.gz"), "-append", "console=ttyAMA0 rdinit=/bin/sh"])
        .args(["-send-after", "# ", "mount -t proc p /proc; uname -sm; grep -E 'Features|CPU part' /proc/cpuinfo; echo 6*7=$((6*7))\\n"])
        .args(["-send-after", "=42", "\\npoweroff -f\\n", "-icount", "30000000000"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "did not power off cleanly:\n{text}");
    for want in ["Run /bin/sh as init process", "Linux aarch64", "fp asimd", "6*7=42", "reboot: Power down"] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
}
