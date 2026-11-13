use ruvm::machine::DevSpec;
use ruvm::{demo, is_linux_image, Arch, LinuxBoot, Machine, Stop};
use std::process::exit;

const USAGE: &str = "\
ruvm 0.8 — a tiny QEMU-like emulator (RISC-V 64 / AArch64)

USAGE:
    ruvm -M <riscv64|aarch64> (-kernel <file> | -demo hello) [options]

OPTIONS:
    -M, -machine, --arch <arch>   riscv64 | aarch64   (aliases: rv64, arm64, arm)
    -kernel, --kernel <file>      Linux Image, ELF64 or raw binary (raw is loaded at start of RAM)
    -initrd <file>                initial ramdisk for a Linux Image
    -append <cmdline>             Linux kernel command line
    -bios <file|none>             firmware run before a Linux Image (e.g. OpenSBI fw_dynamic.bin);
                                  default: ruvm's built-in SBI starts the kernel in S-mode
    -dumpdtb <file>               write the generated device tree and exit
    -demo <name>                  built-in demo program: hello
    -m <MiB>                      RAM size in MiB (default 128)
    -drive file=<img>[,...]       raw disk image as virtio-mmio block device (writes go to the file)
    -drive file=<img> may be repeated (virtio-blk devices in order, then virtio-rng)
    -snapshot                     keep disk writes in memory (image file is not modified)
    -rng, -rng-seed <n>           attach a virtio-rng device (fixed seed for reproducible runs)
    -rtc <secs>                   start the RTC at this Unix time (default: host clock)
    -no-reboot                    exit instead of rebooting when the guest resets
    -input <text>                 type <text> on the guest console at start (\\n = newline)
    -send-after <pattern> <text>  type <text> once the console has printed <pattern> (repeatable)
    -icount <N>                   stop after N instructions
    -d, --trace                   trace every instruction (pc + raw word) to stderr
    -regs                         dump CPU registers on exit
    -serial stdio                 connect host stdin to the guest UART (default: output only)
    -h, --help                    this text

MEMORY MAP (as QEMU virt):
    riscv64   0x0010_0000 test finisher | 0x0200_0000 CLINT | 0x0c00_0000 PLIC
              0x1000_0000 NS16550 UART (PLIC irq 10)        | RAM 0x8000_0000
              0x1000_1000 virtio-mmio block (PLIC irq 1, with -drive)
    aarch64   0x0900_0000 PL011 UART                        | RAM 0x4000_0000

GUEST EXIT:
    riscv64   write 0x5555 to 0x100000, or ecall a7=93 (exit) from M-mode when mtvec==0
    aarch64   HVC/SMC PSCI SYSTEM_OFF (x0=0x84000008), HLT, or SVC x8=93 (exit) when VBAR_EL1==0
";

fn die(msg: &str) -> ! {
    eprintln!("ruvm: {msg}");
    exit(64);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut arch, mut kernel, mut demo_name) = (None, None, None);
    let mut drives: Vec<String> = Vec::new();
    let (mut rng, mut rng_seed, mut rtc, mut no_reboot) = (false, None, None, false);
    let (mut snapshot, mut input) = (false, String::new());
    let mut expects: Vec<(String, String)> = Vec::new();
    let (mut initrd, mut append, mut bios, mut dumpdtb) = (None, String::new(), None, None);
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
            "-drive" | "-hda" | "--disk" => {
                let v = val("-drive");
                // accept QEMU syntax: file=fs.img,if=none,format=raw,...
                let file = v.split(',').find_map(|kv| kv.strip_prefix("file=")).unwrap_or(&v).to_string();
                drives.push(file);
            }
            "-m" => ram = val("-m").parse().unwrap_or_else(|_| die("-m expects a number of MiB")),
            "-icount" => icount = Some(val("-icount").parse().unwrap_or_else(|_| die("bad -icount"))),
            "-d" | "--trace" => trace = true,
            "-regs" => regs = true,
            "-snapshot" => snapshot = true,
            "-rng" => rng = true,
            "-rng-seed" => {
                rng = true;
                rng_seed = Some(val("-rng-seed").parse().unwrap_or_else(|_| die("bad -rng-seed")));
            }
            "-rtc" => rtc = Some(val("-rtc").parse().unwrap_or_else(|_| die("-rtc expects seconds since the epoch"))),
            "-no-reboot" => no_reboot = true,
            "-initrd" => initrd = Some(val("-initrd")),
            "-append" => append = val("-append"),
            "-bios" => bios = Some(val("-bios")).filter(|b| b != "none"),
            "-dumpdtb" => dumpdtb = Some(val("-dumpdtb")),
            "-input" => input += &val("-input").replace("\\n", "\n"),
            "-send-after" => {
                let pat = val("-send-after").replace("\\n", "\n");
                expects.push((pat, val("-send-after").replace("\\n", "\n")));
            }
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
    vm.set_rtc_epoch(rtc);
    vm.trace = trace;
    vm.send_input(input.as_bytes());
    for (pat, text) in &expects {
        vm.send_input_after(pat.as_bytes(), text.as_bytes());
    }
    for path in &drives {
        if let Err(e) = vm.attach_disk(path, snapshot) {
            die(&e);
        }
    }
    if rng {
        vm.attach(DevSpec::Rng(rng_seed)).unwrap_or_else(|e| die(&e));
    }

    let loaded = match (kernel, demo_name) {
        (Some(path), _) => {
            let read = |p: &str| std::fs::read(p).unwrap_or_else(|e| die(&format!("cannot read {p}: {e}")));
            let data = read(&path);
            if is_linux_image(arch, &data) {
                let boot = LinuxBoot {
                    kernel: data,
                    initrd: initrd.as_deref().map(read),
                    cmdline: append.clone(),
                    bios: bios.as_deref().map(read),
                };
                match vm.boot_linux(&boot) {
                    Ok(dtb) => {
                        if let Some(f) = &dumpdtb {
                            std::fs::write(f, &dtb).unwrap_or_else(|e| die(&format!("{f}: {e}")));
                            return;
                        }
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            } else {
                vm.load_image(&data)
            }
        }
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

    // the RTC start time must be known before the board is (re)built
    let mut stop = vm.run(icount);
    while stop == Stop::Reset && !no_reboot {
        if let Err(e) = vm.reset() {
            die(&e);
        }
        stop = vm.run(icount);
    }
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
        Stop::Reset => exit(0), // -no-reboot
    }
}
