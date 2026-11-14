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

/// Boot a Debian kernel with the small initramfs and an ext4 root on virtio-blk
/// (tools/linux/mkroot.sh): the first boot counts itself on the disk and reboots, the
/// second boot must see the persisted counter. Also checks RTC (fixed start time) and virtio-rng.
fn linux_root_reboot(arch: &str, dir: &str, mem: &str, console: &str) {
    let base = env!("CARGO_MANIFEST_DIR");
    let root = format!("{base}/tests/guest/out/linux/root/{dir}");
    let kernel = format!("{base}/tests/guest/out/linux/debian/{dir}/linux");
    if !std::path::Path::new(&format!("{root}/root.ext4")).exists() || !std::path::Path::new(&kernel).exists() {
        eprintln!("skipped: run tools/linux/fetch-debian.sh {dir} and tools/linux/mkroot.sh {dir} first");
        return;
    }
    let tmp = std::env::temp_dir().join(format!("ruvm-root-{dir}-{}.ext4", std::process::id()));
    std::fs::copy(format!("{root}/root.ext4"), &tmp).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ruvm"))
        .args(["-M", arch, "-m", mem, "-kernel", &kernel, "-initrd", &format!("{root}/initramfs.cpio.gz")])
        .args(["-drive", &format!("file={}", tmp.display()), "-rng", "-rng-seed", "7", "-rtc", "1893456000"])
        .args(["-append", &format!("console={console} rdinit=/init ruvm.test=reboot"), "-icount", "60000000000"])
        .output()
        .unwrap();
    std::fs::remove_file(&tmp).ok();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "did not power off cleanly:\n{text}");
    for want in [
        "ruvm root: boot #1",
        "first boot: rebooting",
        "ruvm root: boot #2",
        "second boot: /var/boots persisted = 2",
        "year 2030",
        "hwrng: 16 hex digits",
        "reboot: Power down",
    ] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
}

#[test]
#[ignore]
fn linux_ext4_root_reboot_riscv64() {
    linux_root_reboot("riscv64", "riscv64", "256", "ttyS0");
}

#[test]
#[ignore]
fn linux_ext4_root_reboot_aarch64() {
    linux_root_reboot("aarch64", "arm64", "512", "ttyAMA0");
}

/// Boot with a virtio-net device and the user-mode network: the guest gets a DHCP lease,
/// pings the gateway, fetches a page from a host HTTP server (TCP through the stack), talks to a
/// host TFTP server (UDP), and accepts a connection the host opens through `hostfwd`.
fn linux_network(arch: &str, dir: &str, mem: &str, console: &str) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream, UdpSocket};
    use std::process::Stdio;
    let base = env!("CARGO_MANIFEST_DIR");
    let root = format!("{base}/tests/guest/out/linux/root/{dir}");
    let kernel = format!("{base}/tests/guest/out/linux/debian/{dir}/linux");
    if !std::path::Path::new(&format!("{root}/root.ext4")).exists() || !std::path::Path::new(&kernel).exists() {
        eprintln!("skipped: run tools/linux/fetch-debian.sh {dir} and tools/linux/mkroot.sh {dir} first");
        return;
    }
    // host services: HTTP on a TCP port, UDP echo
    let http = TcpListener::bind("127.0.0.1:0").unwrap();
    let hport = http.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut s in http.incoming().flatten() {
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let body = "hello-from-host";
            let _ = write!(s, "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        }
    });
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    let uport = udp.local_addr().unwrap().port();
    std::thread::spawn(move || {
        // a one-shot TFTP server: answer a read request with a single DATA block
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = udp.recv_from(&mut buf) {
            if n >= 2 && buf[1] == 1 {
                let mut r = vec![0, 3, 0, 1];
                r.extend_from_slice(b"tftp-ok");
                let _ = udp.send_to(&r, from);
            }
        }
    });
    let fwd = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port(); // a free port
    let tmp = std::env::temp_dir().join(format!("ruvm-net-{dir}-{}.ext4", std::process::id()));
    std::fs::copy(format!("{root}/root.ext4"), &tmp).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ruvm"))
        .args(["-M", arch, "-m", mem, "-kernel", &kernel, "-initrd", &format!("{root}/initramfs.cpio.gz")])
        .args(["-drive", &format!("file={}", tmp.display()), "-nic", &format!("user,hostfwd=tcp::{fwd}-:7000")])
        .args(["-append", &format!("console={console} rdinit=/init ruvm.test=net ruvm.port={hport} ruvm.uport={uport}")])
        .args(["-icount", "100000000000"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut text = String::new();
    let mut lines = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    while lines.read_line(&mut line).unwrap_or(0) > 0 {
        if line.starts_with("NET-FWD-READY") {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let mut c = TcpStream::connect(("127.0.0.1", fwd)).expect("connect through hostfwd");
            c.write_all(b"from-host-to-guest\n").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        text.push_str(&line);
        line.clear();
    }
    let status = child.wait().unwrap();
    std::fs::remove_file(&tmp).ok();
    assert!(status.success(), "did not power off cleanly:\n{text}");
    for want in [
        "inet addr:10.0.2.15",
        "NET-PING-OK",
        "NET-HTTP: hello-from-host",
        "NET-UDP: tftp-ok",
        "NET-FWD-GOT: from-host-to-guest",
    ] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
}

#[test]
#[ignore]
fn linux_user_network_riscv64() {
    linux_network("riscv64", "riscv64", "256", "ttyS0");
}

#[test]
#[ignore]
fn linux_user_network_aarch64() {
    linux_network("aarch64", "arm64", "512", "ttyAMA0");
}

fn run_elf_smp(arch: &str, name: &str, cpus: &str) -> String {
    let path = format!("{}/tests/guest/prebuilt/{}", env!("CARGO_MANIFEST_DIR"), name);
    let out = Command::new(env!("CARGO_BIN_EXE_ruvm"))
        .args(["-M", arch, "-smp", cpus, "-kernel", &path, "-icount", "2000000000"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

/// Four harts running the same program (as with QEMU `-bios none -smp 4`): AMO counter,
/// LR/SC spinlock and CLINT software interrupts; output equal to qemu-system-riscv64.
#[test]
fn riscv_smp_matches_qemu_system() {
    assert_eq!(run_elf_smp("riscv64", "rv_smp.elf", "4"), expected("rv_smp.expected"));
}

/// PSCI CPU_ON / error codes, LDXR/STXR and LSE atomics across CPUs, GIC SGIs with source
/// ids and target filters, broadcast TLBI; output equal to qemu-system-aarch64 -smp 4.
#[test]
fn aarch64_smp_matches_qemu_system() {
    assert_eq!(run_elf_smp("aarch64", "a64_smp.elf", "4"), expected("a64_smp.expected"));
}

/// xv6 (its own SMP scheduler and spinlocks) on two harts: both start and the shell works.
#[test]
fn xv6_boots_on_two_harts() {
    let dir = format!("{}/tests/guest/prebuilt/xv6", env!("CARGO_MANIFEST_DIR"));
    let out = Command::new(env!("CARGO_BIN_EXE_ruvm"))
        .args(["-M", "riscv64", "-smp", "2", "-m", "128", "-kernel", &format!("{dir}/kernel")])
        .args(["-drive", &format!("file={dir}/fs.img"), "-snapshot"])
        .args(["-send-after", "$ ", "echo smp-ok\\n", "-icount", "2500000000"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    for want in ["hart 1 starting", "smp-ok"] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
}

fn linux_smp(arch: &str, dir: &str, console: &str) {
    let base = env!("CARGO_MANIFEST_DIR");
    let root = format!("{base}/tests/guest/out/linux/root/{dir}");
    let kernel = format!("{base}/tests/guest/out/linux/debian/{dir}/linux");
    if !std::path::Path::new(&format!("{root}/initramfs.cpio.gz")).exists() || !std::path::Path::new(&kernel).exists() {
        eprintln!("skipped: run tools/linux/fetch-debian.sh {dir} and tools/linux/mkroot.sh {dir} first");
        return;
    }
    let tmp = std::env::temp_dir().join(format!("ruvm-smp-{dir}-{}.ext4", std::process::id()));
    std::fs::copy(format!("{root}/root.ext4"), &tmp).unwrap();
    let script = "echo NPROC=$(nproc); for i in 1 2 3 4; do (n=0; while [ $n -lt 2000 ]; do n=$((n+1)); done; echo job$i-done) & done; wait; echo ALLDONE\\n";
    let out = Command::new(env!("CARGO_BIN_EXE_ruvm"))
        .args(["-M", arch, "-m", "512", "-smp", "4", "-kernel", &kernel, "-initrd", &format!("{root}/initramfs.cpio.gz")])
        .args(["-drive", &format!("file={}", tmp.display()), "-append", &format!("console={console} rdinit=/init")])
        .args(["-send-after", "# ", script, "-send-after", "ALLDONE", "\\npoweroff -f\\n", "-icount", "100000000000"])
        .output()
        .unwrap();
    std::fs::remove_file(&tmp).ok();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "did not power off cleanly:\n{text}");
    for want in ["smp: Brought up 1 node, 4 CPUs", "NPROC=4", "job1-done", "job4-done", "reboot: Power down"] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
    assert!(!text.contains("soft lockup") && !text.contains("rcu: INFO"), "stall detected:\n{text}");
}

#[test]
#[ignore]
fn linux_smp_riscv64() {
    linux_smp("riscv64", "riscv64", "ttyS0");
}

#[test]
#[ignore]
fn linux_smp_aarch64() {
    linux_smp("aarch64", "arm64", "ttyAMA0");
}

/// Zba / Zbb / Zbc / Zbs / Zicond over edge-case operands (66 instruction forms).
#[test]
fn riscv_bitmanip_matches_qemu_system() {
    same_as_qemu("riscv64", "rv_bitmanip.elf");
}

/// PMP (OFF/TOR/NAPOT regions, a locked region that also binds M-mode), Zicbom/Zicboz with
/// menvcfg gating, Svinval and Sv48 translation, checked trap by trap against QEMU.
#[test]
fn riscv_pmp_cbo_sv48_matches_qemu_system() {
    same_as_qemu("riscv64", "rv_ext.elf");
}

// ---------------------------------------------------------------- user-mode emulation (-user)

fn run_user(prog: &str, args: &[&str], stdin: &[u8]) -> (i32, String) {
    use std::io::Write;
    use std::process::Stdio;
    let path = format!("{}/{}", env!("CARGO_MANIFEST_DIR"), prog);
    let mut child = Command::new(env!("CARGO_BIN_EXE_ruvm"))
        .arg("-user")
        .arg(&path)
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A freestanding program using raw syscalls: argv/auxv, brk, mmap, FP, files, uname, clock, ENOSYS, exit code.
#[test]
fn user_mode_freestanding_program() {
    for (elf, machine) in [("user_rv.elf", "riscv64"), ("user_a64.elf", "aarch64")] {
        let (code, out) = run_user(&format!("tests/guest/prebuilt/{elf}"), &["one", "two words"], b"");
        assert_eq!(code, 3, "{elf}:\n{out}");
        let want = format!(
            "hello from user mode\nargc=3\narg: one\narg: two words\npagesz=4096\nrandom=1\nbrk grew=12288\n\
             mmap sum=34359607296\nmunmap=0\nsqrt2 * 1e9=1414213562\nmagic: ELF\nopen missing=-2\n\
             sysname: Linux\nmachine: {machine}\nclock ok=1\n\
             pipe2=0\npipe read=4\nping\npipe widowed=-32\nF_DUPFD >= 20=1\ndup read=3\n\
             statx=0\nstatx size > 0=1\nchdir=0\nrename missing=-2\n\
             fork pipe read=10\nfrom child\nwait4 pid ok=1\nchild status=1792\nwait4 no children=-10\n\
             exec'd image\nexec status=2304\nexec missing=-2\n\
             sigaction=0\nhandler sig=10\nkill=0\nafter kill got=10\nblocked got=0\nhandler sig=10\nunblocked got=10\n\
             ignored SIGUSR2 survived\nSIGTERM status=36608\nsegv handler 11\nSIGSEGV status=1280\n\
             threads created=1\ncounter=2000\nwaiter saw=1\nthread tid differs=1\nfutex timeout=-110\nfutex value mismatch=-11\nunknown syscall=-38\n"
        );
        assert_eq!(out, want, "{elf}");
    }
}

/// A null-pointer access faults (exit status 139) after the program has started.
#[test]
fn user_mode_null_pointer_faults() {
    for elf in ["user_rv.elf", "user_a64.elf"] {
        let (code, out) = run_user(&format!("tests/guest/prebuilt/{elf}"), &["null"], b"");
        assert_eq!((code, out.as_str()), (139, "hello from user mode\n"), "{elf}");
    }
}

/// Real glibc-static programs (Debian's busybox): file I/O, pipes, sorting, hashing.
/// Needs `tools/linux/fetch-busybox.sh`.
#[test]
#[ignore]
fn user_mode_busybox() {
    for arch in ["riscv64", "arm64"] {
        let bb = format!("tests/guest/out/user/{arch}/busybox");
        let (c, o) = run_user(&bb, &["echo", "hello", "busybox"], b"");
        assert_eq!((c, o.as_str()), (0, "hello busybox\n"), "{arch}");
        let (c, o) = run_user(&bb, &["tr", "a-z", "A-Z"], b"x y z\n");
        assert_eq!((c, o.as_str()), (0, "X Y Z\n"), "{arch}");
        let (c, o) = run_user(&bb, &["sha256sum", "Cargo.toml"], b"");
        assert_eq!(c, 0, "{arch}");
        let sum = Command::new("shasum").args(["-a", "256", "Cargo.toml"]).current_dir(env!("CARGO_MANIFEST_DIR")).output().unwrap();
        assert_eq!(o.split_whitespace().next(), String::from_utf8_lossy(&sum.stdout).split_whitespace().next(), "{arch}");
        let (c, o) = run_user(&bb, &["uname", "-m"], b"");
        assert_eq!((c, o.trim()), (0, if arch == "arm64" { "aarch64" } else { "riscv64" }));
        let (c, _) = run_user(&bb, &["false"], b"");
        assert_eq!(c, 1, "{arch}");
        // a shell: fork, execve of applets, pipelines, redirection, exit status
        let script = "echo $((6*7)); uname -m | tr a-z A-Z; seq 1 5 | sort -r | tr '\\n' ' '; echo; echo data > /dev/null; sh -c 'exit 3'; echo rc=$?";
        let (c, o) = run_user(&bb, &["sh", "-c", script], b"");
        let m = if arch == "arm64" { "AARCH64" } else { "RISCV64" };
        assert_eq!((c, o.as_str()), (0, format!("42\n{m}\n5 4 3 2 1 \nrc=3\n").as_str()), "{arch}");
        // glibc signal handlers: trap + kill to self, a child killed by SIGTERM
        let trap = "trap 'echo trapped' USR1; kill -USR1 $$; echo after; sh -c 'kill -TERM $$'; echo rc=$?";
        let (c, o) = run_user(&bb, &["sh", "-c", trap], b"");
        assert_eq!((c, o.as_str()), (0, "trapped\nafter\nrc=143\n"), "{arch}");
    }
}
