use ruvm::{demo, Arch, Machine, Stop};
use std::process::exit;

const USAGE: &str = "\
ruvm — a tiny QEMU-like emulator (RISC-V 64 / AArch64)

USAGE:
    ruvm -M <riscv64|aarch64> (-kernel <file> | -demo hello) [options]

OPTIONS:
    -M, -machine, --arch <arch>   riscv64 | aarch64   (aliases: rv64, arm64, arm)
    -kernel, --kernel <file>      ELF64 or raw binary (raw is loaded at start of RAM)
    -demo <name>                  built-in demo program: hello
    -m <MiB>                      RAM size in MiB (default 128)
    -icount <N>                   stop after N instructions
    -d, --trace                   trace every instruction (pc + raw word) to stderr
    -regs                         dump CPU registers on exit
    -serial stdio                 connect host stdin to the guest UART (default: output only)
    -h, --help                    this text

MEMORY MAP:
    riscv64   0x0010_0000 test finisher | 0x1000_0000 NS16550 UART | RAM 0x8000_0000
    aarch64   0x0900_0000 PL011 UART                               | RAM 0x4000_0000

GUEST EXIT:
    riscv64   write 0x5555 to 0x100000, or ecall a7=93 (exit) when mtvec==0
    aarch64   HVC/SMC PSCI SYSTEM_OFF (x0=0x84000008), SVC x8=93 (exit), or HLT
";

fn die(msg: &str) -> ! {
    eprintln!("ruvm: {msg}");
    exit(64);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut arch, mut kernel, mut demo_name) = (None, None, None);
    let (mut ram, mut icount, mut trace, mut regs, mut interactive) = (128usize, None, false, false, false);

    while let Some(a) = args.next() {
        let mut val = |name: &str| args.next().unwrap_or_else(|| die(&format!("{name} needs a value")));
        match a.as_str() {
            "-M" | "-machine" | "--arch" | "-arch" => {
                let v = val("-M");
                arch = Some(Arch::parse(&v).unwrap_or_else(|| die(&format!("unknown machine '{v}'"))));
            }
            "-kernel" | "--kernel" => kernel = Some(val("-kernel")),
            "-demo" | "--demo" => demo_name = Some(val("-demo")),
            "-m" => ram = val("-m").parse().unwrap_or_else(|_| die("-m expects a number of MiB")),
            "-icount" => icount = Some(val("-icount").parse().unwrap_or_else(|_| die("bad -icount"))),
            "-d" | "--trace" => trace = true,
            "-regs" => regs = true,
            "-serial" => interactive = val("-serial") == "stdio",
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            other => die(&format!("unknown option '{other}' (try --help)")),
        }
    }

    let arch = arch.unwrap_or_else(|| die("missing -M <riscv64|aarch64>"));
    let mut vm = Machine::new(arch, ram, interactive);
    vm.trace = trace;

    let loaded = match (kernel, demo_name) {
        (Some(path), _) => match std::fs::read(&path) {
            Ok(data) => vm.load_image(&data),
            Err(e) => die(&format!("cannot read {path}: {e}")),
        },
        (None, Some(name)) if name == "hello" => {
            let img = match arch {
                Arch::Riscv64 => demo::rv::hello(),
                Arch::Aarch64 => demo::a64::hello(),
            };
            vm.load_raw(arch.ram_base(), &img)
        }
        (None, Some(name)) => die(&format!("unknown demo '{name}'")),
        (None, None) => die("need -kernel <file> or -demo hello"),
    };
    if let Err(e) = loaded {
        die(&e);
    }

    let stop = vm.run(icount);
    if regs || matches!(stop, Stop::Fatal(_)) {
        eprintln!("--- {} registers ---\n{}", vm.cpu.name(), vm.cpu.dump());
    }
    match stop {
        Stop::Halt(code) => exit(code),
        Stop::Limit => {
            eprintln!("ruvm: instruction limit reached after {} instructions", vm.insns);
            exit(2)
        }
        Stop::Fatal(msg) => {
            eprintln!("ruvm: fatal: {msg}");
            exit(1)
        }
    }
}
