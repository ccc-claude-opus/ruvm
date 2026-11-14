//! Tiny "semihosting" layer: Linux-style syscalls shared by both guests.
//!
//! RISC-V: `ecall` with a7 = number (only when mtvec == 0, i.e. no trap handler).
//! AArch64: `svc #0` with x8 = number.
//!   93 = exit(code), 64 = write(fd, buf, len)

use crate::bus::Bus;
use std::io::Write;

pub enum Host {
    Ret(u64),
    /// Return a value and switch the stack pointer (a `clone` child with its own stack).
    RetSp(u64, u64),
    /// `execve` succeeded: the machine replaces the process image (see `Stop::Exec`).
    Exec,
    Exit(i32),
}

pub fn syscall(bus: &mut Bus, nr: u64, a: [u64; 3]) -> Host {
    match nr {
        93 => Host::Exit(a[0] as i32),
        64 => {
            let (fd, buf, len) = (a[0], a[1], a[2]);
            let mut data = Vec::with_capacity(len.min(1 << 20) as usize);
            for k in 0..len {
                match bus.read(buf.wrapping_add(k), 1) {
                    Ok(b) => data.push(b as u8),
                    Err(_) => return Host::Ret((-14i64) as u64), // EFAULT
                }
            }
            let ok = match fd {
                1 => {
                    let mut o = std::io::stdout().lock();
                    o.write_all(&data).and_then(|_| o.flush()).is_ok()
                }
                2 => std::io::stderr().write_all(&data).is_ok(),
                _ => return Host::Ret((-9i64) as u64), // EBADF
            };
            Host::Ret(if ok { len } else { (-5i64) as u64 })
        }
        _ => Host::Ret((-38i64) as u64), // ENOSYS
    }
}
