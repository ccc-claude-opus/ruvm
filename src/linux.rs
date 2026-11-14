//! User-mode emulation ("proxy kernel"): run one static Linux ELF directly, like `spike pk`
//! or `qemu-user`. The guest runs without an OS; its Linux system calls (`ecall` / `svc #0`)
//! are served here by the host. RISC-V and AArch64 share the asm-generic syscall table.
//!
//! Memory is one flat region starting at 64 KiB (so null-pointer accesses fault): the program at its link addresses,
//! `brk` heap above it, `mmap` regions growing down from below the stack, and the stack at the top.
//! Not supported: threads/`clone`/`fork`/`exec`, signals, dynamic linking, mapping of files shared with the guest.

use crate::bus::Bus;
use crate::machine::Arch;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::os::unix::io::FromRawFd;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const PAGE: u64 = 4096;
/// Guest RAM starts here; the first 64 KiB are unmapped so that null-pointer accesses fault.
pub const RAM_BASE: u64 = 0x1_0000;
const STACK_SIZE: u64 = 8 << 20;
const STACK_GAP: u64 = 8 << 20; // unused space between the stack and the mmap area

enum Fd {
    Stdin,
    Stdout,
    Stderr,
    File(File),
    Dir(PathBuf, Vec<(String, u8, u64)>, usize),
}

pub struct LinuxUser {
    arch: Arch,
    exe: String,
    brk_start: u64,
    brk: u64,
    mmap_next: u64,
    free: Vec<(u64, u64)>,
    fds: Vec<Option<Fd>>,
    rng: u64,
    start: Instant,
    /// Print every system call to stderr (`RUVM_STRACE=1`).
    strace: bool,
    /// Descriptors closed by `execve`.
    cloexec: HashSet<usize>,
    /// A new process image prepared by `execve`, installed by the machine.
    pub pending_exec: Option<ExecReq>,
}

/// Result of a successful `execve`: the new memory image (with the inherited descriptors).
pub struct ExecReq {
    pub bus: Bus,
    pub entry: u64,
    pub sp: u64,
}

extern "C" {
    fn fork() -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn pipe(fds: *mut i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}

/// What a system call did.
pub enum Sys {
    Ret(i64),
    Exit(i32),
    /// `execve` succeeded.
    Exec,
    /// Return a value and set the stack pointer (`clone` with a child stack).
    RetSp(i64, u64),
}

/// Serve a system call for the CPU: a `host::Host` result (exit, or the return value).
pub fn syscall(bus: &mut Bus, nr: u64, args: [u64; 6]) -> crate::host::Host {
    let mut u = bus.user.take().expect("user-mode state");
    let r = u.syscall(bus, nr, args);
    if u.strace {
        match &r {
            Sys::Ret(v) => eprintln!("syscall {nr}({:#x}, {:#x}, {:#x}, {:#x}) = {v}", args[0], args[1], args[2], args[3]),
            Sys::Exit(c) => eprintln!("syscall {nr}({:#x}) exit {c}", args[0]),
            Sys::Exec => eprintln!("syscall {nr} execve"),
            Sys::RetSp(v, sp) => eprintln!("syscall {nr} = {v} (sp {sp:#x})"),
        }
    }
    bus.user = Some(u);
    match r {
        Sys::Ret(v) => crate::host::Host::Ret(v as u64),
        Sys::Exit(c) => crate::host::Host::Exit(c),
        Sys::Exec => crate::host::Host::Exec,
        Sys::RetSp(v, sp) => crate::host::Host::RetSp(v as u64, sp),
    }
}

fn align_up(v: u64) -> u64 {
    (v + PAGE - 1) & !(PAGE - 1)
}

fn errno(e: &std::io::Error) -> i64 {
    -(match e.raw_os_error() {
        Some(35) => 11, // EAGAIN
        Some(62) => 40, // ELOOP
        Some(63) => 36, // ENAMETOOLONG
        Some(66) => 39, // ENOTEMPTY
        Some(n) if (1..=34).contains(&n) => n as i64,
        _ => match e.kind() {
            std::io::ErrorKind::NotFound => 2,
            std::io::ErrorKind::PermissionDenied => 13,
            std::io::ErrorKind::AlreadyExists => 17,
            std::io::ErrorKind::InvalidInput => 22,
            _ => 5,
        },
    })
}

const EBADF: i64 = -9;
const ENOENT: i64 = -2;
const ENOMEM: i64 = -12;
const EFAULT: i64 = -14;
const EINVAL: i64 = -22;
const ENOTTY: i64 = -25;
const ENOSYS: i64 = -38;

impl LinuxUser {
    pub fn new(arch: Arch, exe: &str, brk_start: u64, stack_low: u64) -> Self {
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64);
        LinuxUser {
            arch,
            exe: std::fs::canonicalize(exe).map_or_else(|_| exe.to_string(), |p| p.to_string_lossy().into_owned()),
            brk_start,
            brk: brk_start,
            mmap_next: stack_low,
            free: Vec::new(),
            fds: vec![Some(Fd::Stdin), Some(Fd::Stdout), Some(Fd::Stderr)],
            rng: seed | 1,
            start: Instant::now(),
            strace: std::env::var_os("RUVM_STRACE").is_some(),
            cloexec: HashSet::new(),
            pending_exec: None,
        }
    }

    fn rand(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    fn dup_fd(&self, fd: usize) -> Result<Fd, i64> {
        Ok(match self.fds.get(fd) {
            Some(Some(Fd::Stdin)) => Fd::Stdin,
            Some(Some(Fd::Stdout)) => Fd::Stdout,
            Some(Some(Fd::Stderr)) => Fd::Stderr,
            Some(Some(Fd::File(f))) => Fd::File(f.try_clone().map_err(|e| errno(&e))?),
            Some(Some(Fd::Dir(p, e, n))) => Fd::Dir(p.clone(), e.clone(), *n),
            _ => return Err(EBADF),
        })
    }

    fn close_fd(&mut self, fd: usize) -> i64 {
        match self.fds.get_mut(fd) {
            Some(slot @ Some(_)) => {
                *slot = None;
                self.cloexec.remove(&fd);
                0
            }
            _ => EBADF,
        }
    }

    fn alloc_fd(&mut self, fd: Fd) -> i64 {
        self.alloc_fd_from(fd, 0)
    }

    fn alloc_fd_from(&mut self, fd: Fd, min: usize) -> i64 {
        while self.fds.len() < min {
            self.fds.push(None);
        }
        let at = self.fds.iter().skip(min).position(|f| f.is_none()).map(|i| i + min);
        if let Some(i) = at {
            self.cloexec.remove(&i);
        }
        match at {
            Some(i) => {
                self.fds[i] = Some(fd);
                i as i64
            }
            None => {
                self.fds.push(Some(fd));
                self.fds.len() as i64 - 1
            }
        }
    }

    /// Reserve `len` bytes (page multiple) of zeroed memory.
    fn alloc(&mut self, bus: &mut Bus, len: u64) -> Option<u64> {
        if let Some(i) = self.free.iter().position(|&(_, l)| l >= len) {
            let (a, l) = self.free[i];
            if l == len {
                self.free.remove(i);
            } else {
                self.free[i] = (a + len, l - len);
            }
            bus.guest_bytes_mut(a, len as usize)?.fill(0);
            return Some(a);
        }
        let a = self.mmap_next.checked_sub(len)?;
        if a < self.brk + PAGE {
            return None;
        }
        self.mmap_next = a;
        Some(a)
    }

    fn release(&mut self, addr: u64, len: u64) {
        self.free.push((addr, len));
        while let Some(i) = self.free.iter().position(|&(a, _)| a == self.mmap_next) {
            self.mmap_next += self.free.remove(i).1;
        }
    }

    fn cstr(bus: &Bus, mut addr: u64) -> Option<String> {
        let mut v = Vec::new();
        loop {
            let b = *bus.guest_bytes(addr, 1)?.first()?;
            if b == 0 {
                return Some(String::from_utf8_lossy(&v).into_owned());
            }
            v.push(b);
            addr += 1;
            if v.len() > 4096 {
                return None;
            }
        }
    }

    fn path(&self, bus: &Bus, dirfd: i64, p: u64) -> Result<PathBuf, i64> {
        let s = Self::cstr(bus, p).ok_or(EFAULT)?;
        let path = if s == "/proc/self/exe" { PathBuf::from(&self.exe) } else { PathBuf::from(s) };
        if path.is_absolute() || dirfd == -100 {
            return Ok(path);
        }
        match self.fds.get(dirfd as usize) {
            Some(Some(Fd::Dir(d, _, _))) => Ok(d.join(path)),
            _ => Err(EBADF),
        }
    }

    fn stat(&self, bus: &mut Bus, md: &std::fs::Metadata, out: u64) -> i64 {
        let mut b = [0u8; 128];
        let put64 = |b: &mut [u8; 128], o: usize, v: u64| b[o..o + 8].copy_from_slice(&v.to_le_bytes());
        let put32 = |b: &mut [u8; 128], o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
        put64(&mut b, 0, md.dev());
        put64(&mut b, 8, md.ino());
        put32(&mut b, 16, md.mode());
        put32(&mut b, 20, md.nlink() as u32);
        put32(&mut b, 24, md.uid());
        put32(&mut b, 28, md.gid());
        put64(&mut b, 32, md.rdev());
        put64(&mut b, 48, md.size());
        put32(&mut b, 56, md.blksize() as u32);
        put64(&mut b, 64, md.blocks());
        put64(&mut b, 72, md.atime() as u64);
        put64(&mut b, 80, md.atime_nsec() as u64);
        put64(&mut b, 88, md.mtime() as u64);
        put64(&mut b, 96, md.mtime_nsec() as u64);
        put64(&mut b, 104, md.ctime() as u64);
        put64(&mut b, 112, md.ctime_nsec() as u64);
        match bus.guest_bytes_mut(out, 128) {
            Some(d) => {
                d.copy_from_slice(&b);
                0
            }
            None => EFAULT,
        }
    }

    fn stat_tty(&self, bus: &mut Bus, out: u64) -> i64 {
        let mut b = [0u8; 128];
        b[16..20].copy_from_slice(&0o20620u32.to_le_bytes()); // S_IFCHR | 0620
        b[20..24].copy_from_slice(&1u32.to_le_bytes());
        b[56..60].copy_from_slice(&1024u32.to_le_bytes());
        match bus.guest_bytes_mut(out, 128) {
            Some(d) => {
                d.copy_from_slice(&b);
                0
            }
            None => EFAULT,
        }
    }

    fn put(bus: &mut Bus, addr: u64, data: &[u8]) -> i64 {
        match bus.guest_bytes_mut(addr, data.len()) {
            Some(d) => {
                d.copy_from_slice(data);
                0
            }
            None => EFAULT,
        }
    }

    /// Copy to guest memory; the byte count on success.
    fn put_n(bus: &mut Bus, addr: u64, data: &[u8]) -> i64 {
        match Self::put(bus, addr, data) {
            0 => data.len() as i64,
            e => e,
        }
    }

    fn write_fd(&mut self, fd: usize, data: &[u8], at: Option<u64>) -> i64 {
        let r = match self.fds.get_mut(fd) {
            Some(Some(Fd::Stdout)) => {
                let mut o = std::io::stdout().lock();
                o.write_all(data).and_then(|_| o.flush()).map(|_| data.len())
            }
            Some(Some(Fd::Stderr)) => std::io::stderr().write_all(data).map(|_| data.len()),
            Some(Some(Fd::File(f))) => match at {
                Some(off) => f.write_at(data, off),
                None => f.write(data),
            },
            Some(Some(Fd::Dir(..))) => return -21, // EISDIR
            _ => return EBADF,
        };
        r.map_or_else(|e| errno(&e), |n| n as i64)
    }

    fn read_fd(&mut self, fd: usize, buf: &mut [u8], at: Option<u64>) -> i64 {
        let r = match self.fds.get_mut(fd) {
            Some(Some(Fd::Stdin)) => {
                // raw descriptor 0: no buffering, so a forked child does not see a copy of buffered input
                let mut f = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(0) });
                f.read(buf)
            }
            Some(Some(Fd::File(f))) => match at {
                Some(off) => f.read_at(buf, off),
                None => f.read(buf),
            },
            Some(Some(Fd::Dir(..))) => return -21,
            _ => return EBADF,
        };
        r.map_or_else(|e| errno(&e), |n| n as i64)
    }

    fn sys_openat(&mut self, bus: &mut Bus, a: [u64; 6]) -> i64 {
        let path = match self.path(bus, a[0] as i64, a[1]) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let flags = a[2];
        let (acc, creat, excl, trunc, append) = (flags & 3, flags & 0o100 != 0, flags & 0o200 != 0, flags & 0o1000 != 0, flags & 0o2000 != 0);
        let dir_flag = if self.arch == Arch::Aarch64 { 0o40000 } else { 0o200000 };
        if path == std::path::Path::new("/proc/self/exe") {
            return match File::open(&self.exe) {
                Ok(f) => self.alloc_fd(Fd::File(f)),
                Err(e) => errno(&e),
            };
        }
        if path.is_dir() && (acc == 0 || flags & dir_flag != 0) {
            let mut ents = vec![(".".to_string(), 4u8, 0u64), ("..".to_string(), 4, 0)];
            match std::fs::read_dir(&path) {
                Ok(rd) => {
                    for e in rd.flatten() {
                        let ty = e.file_type().map_or(0, |t| if t.is_dir() { 4 } else if t.is_symlink() { 10 } else { 8 });
                        ents.push((e.file_name().to_string_lossy().into_owned(), ty, e.metadata().map_or(0, |m| m.ino())));
                    }
                }
                Err(e) => return errno(&e),
            }
            let fd = self.alloc_fd(Fd::Dir(path, ents, 0));
            if flags & 0o2000000 != 0 {
                self.cloexec.insert(fd as usize);
            }
            return fd;
        }
        let mut o = OpenOptions::new();
        match acc {
            0 => o.read(true),
            1 => o.write(true),
            _ => o.read(true).write(true),
        };
        if append {
            o.append(true);
        }
        if trunc {
            o.truncate(true);
        }
        if creat && excl {
            o.create_new(true);
        } else if creat {
            o.create(true);
        }
        o.mode(a[3] as u32 & 0o7777);
        match o.open(&path) {
            Ok(f) => {
                let fd = self.alloc_fd(Fd::File(f));
                if flags & 0o2000000 != 0 {
                    self.cloexec.insert(fd as usize);
                }
                fd
            }
            Err(e) => errno(&e),
        }
    }

    fn sys_mmap(&mut self, bus: &mut Bus, a: [u64; 6]) -> i64 {
        let (addr, len, flags, fd, off) = (a[0], align_up(a[1]), a[3], a[4] as i64, a[5]);
        if len == 0 {
            return EINVAL;
        }
        let fixed = flags & 0x10 != 0;
        let anon = flags & 0x20 != 0;
        let at = if fixed {
            if addr % PAGE != 0 || bus.guest_bytes(addr, len as usize).is_none() {
                return ENOMEM;
            }
            bus.guest_bytes_mut(addr, len as usize).unwrap().fill(0);
            addr
        } else {
            match self.alloc(bus, len) {
                Some(p) => p,
                None => return ENOMEM,
            }
        };
        if !anon {
            let mut data = vec![0u8; len as usize];
            let n = match self.fds.get(fd as usize) {
                Some(Some(Fd::File(f))) => f.read_at(&mut data, off).unwrap_or(0),
                _ => return EBADF,
            };
            bus.guest_bytes_mut(at, n).unwrap().copy_from_slice(&data[..n]);
        }
        at as i64
    }

    fn sys_getdents(&mut self, bus: &mut Bus, fd: usize, buf: u64, cap: u64) -> i64 {
        let Some(Some(Fd::Dir(_, ents, pos))) = self.fds.get_mut(fd) else { return EBADF };
        let mut out = Vec::new();
        while *pos < ents.len() {
            let (name, ty, ino) = &ents[*pos];
            let reclen = (19 + name.len() + 1 + 7) & !7;
            if out.len() + reclen > cap as usize {
                break;
            }
            let start = out.len();
            out.extend_from_slice(&ino.to_le_bytes());
            out.extend_from_slice(&((*pos + 1) as u64).to_le_bytes());
            out.extend_from_slice(&(reclen as u16).to_le_bytes());
            out.push(*ty);
            out.extend_from_slice(name.as_bytes());
            out.resize(start + reclen, 0);
            *pos += 1;
        }
        if out.is_empty() && *pos < ents.len() {
            return EINVAL;
        }
        match Self::put(bus, buf, &out) {
            0 => out.len() as i64,
            e => e,
        }
    }

    pub fn syscall(&mut self, bus: &mut Bus, nr: u64, a: [u64; 6]) -> Sys {
        let r = match nr {
            93 | 94 => return Sys::Exit(a[0] as i32),
            // read / write / readv / writev / pread / pwrite
            63 | 67 => {
                let mut buf = vec![0u8; a[2].min(1 << 24) as usize];
                let n = self.read_fd(a[0] as usize, &mut buf, (nr == 67).then_some(a[3]));
                if n > 0 { Self::put_n(bus, a[1], &buf[..n as usize]) } else { n }
            }
            64 | 68 => match bus.guest_bytes(a[1], a[2] as usize) {
                Some(d) => {
                    let d = d.to_vec();
                    self.write_fd(a[0] as usize, &d, (nr == 68).then_some(a[3]))
                }
                None => EFAULT,
            },
            65 | 66 => {
                let mut total = 0i64;
                for k in 0..a[2] {
                    let (Some(base), Some(len)) = (bus.guest_bytes(a[1] + k * 16, 8), bus.guest_bytes(a[1] + k * 16 + 8, 8)) else { return Sys::Ret(EFAULT) };
                    let (base, len) = (u64::from_le_bytes(base.try_into().unwrap()), u64::from_le_bytes(len.try_into().unwrap()));
                    let n = if nr == 66 {
                        match bus.guest_bytes(base, len as usize) {
                            Some(d) => {
                                let d = d.to_vec();
                                self.write_fd(a[0] as usize, &d, None)
                            }
                            None => EFAULT,
                        }
                    } else {
                        let mut buf = vec![0u8; len as usize];
                        let n = self.read_fd(a[0] as usize, &mut buf, None);
                        if n > 0 { Self::put_n(bus, base, &buf[..n as usize]) } else { n }
                    };
                    if n < 0 {
                        if total == 0 {
                            total = n;
                        }
                        break;
                    }
                    total += n;
                    if (n as u64) < len {
                        break;
                    }
                }
                total
            }
            56 => self.sys_openat(bus, a),
            57 => self.close_fd(a[0] as usize),
            23 | 24 => match self.dup_fd(a[0] as usize) {
                Ok(f) if nr == 23 => self.alloc_fd(f),
                Ok(f) => {
                    let t = a[1] as usize;
                    if t == a[0] as usize {
                        EINVAL
                    } else {
                        self.close_fd(t);
                        while self.fds.len() <= t {
                            self.fds.push(None);
                        }
                        self.fds[t] = Some(f);
                        if a[2] & 0o2000000 != 0 {
                            self.cloexec.insert(t);
                        }
                        t as i64
                    }
                }
                Err(e) => e,
            },
            59 => {
                // pipe2(fds, flags): a real host pipe, so it also works between forked processes
                let mut f = [0i32; 2];
                if unsafe { pipe(f.as_mut_ptr()) } != 0 {
                    return Sys::Ret(errno(&std::io::Error::last_os_error()));
                }
                let (r, w) = unsafe { (File::from_raw_fd(f[0]), File::from_raw_fd(f[1])) };
                let (r, w) = (self.alloc_fd(Fd::File(r)), self.alloc_fd(Fd::File(w)));
                if a[1] & 0o2000000 != 0 {
                    self.cloexec.extend([r as usize, w as usize]);
                }
                match Self::put(bus, a[0], &[(r as i32).to_le_bytes(), (w as i32).to_le_bytes()].concat()) {
                    0 => 0,
                    e => e,
                }
            }
            25 => match a[1] {
                0 | 1030 => match self.dup_fd(a[0] as usize) {
                    Ok(f) => {
                        let fd = self.alloc_fd_from(f, a[2] as usize);
                        if a[1] == 1030 {
                            self.cloexec.insert(fd as usize);
                        }
                        fd
                    }
                    Err(e) => e,
                },
                1 => self.cloexec.contains(&(a[0] as usize)) as i64,
                2 => {
                    if a[2] & 1 != 0 {
                        self.cloexec.insert(a[0] as usize);
                    } else {
                        self.cloexec.remove(&(a[0] as usize));
                    }
                    0
                }
                3 | 4 => 0,
                _ => EINVAL,
            },
            62 => match self.fds.get_mut(a[0] as usize) {
                Some(Some(Fd::File(f))) => {
                    let pos = match a[2] {
                        0 => SeekFrom::Start(a[1]),
                        1 => SeekFrom::Current(a[1] as i64),
                        _ => SeekFrom::End(a[1] as i64),
                    };
                    f.seek(pos).map_or_else(|e| errno(&e), |p| p as i64)
                }
                Some(Some(_)) => -29, // ESPIPE
                _ => EBADF,
            },
            61 => self.sys_getdents(bus, a[0] as usize, a[1], a[2]),
            79 => {
                // newfstatat(dirfd, path, stat, flags)
                let empty = Self::cstr(bus, a[1]).is_some_and(|s| s.is_empty()) && a[3] & 0x1000 != 0;
                if empty {
                    return Sys::Ret(self.sys_fstat(bus, a[0] as usize, a[2]));
                }
                match self.path(bus, a[0] as i64, a[1]) {
                    Ok(p) => {
                        let md = if a[3] & 0x100 != 0 { std::fs::symlink_metadata(&p) } else { std::fs::metadata(&p) };
                        md.map_or_else(|e| errno(&e), |m| self.stat(bus, &m, a[2]))
                    }
                    Err(e) => e,
                }
            }
            80 => self.sys_fstat(bus, a[0] as usize, a[1]),
            48 | 439 => match self.path(bus, a[0] as i64, a[1]) {
                Ok(p) => std::fs::metadata(&p).map_or_else(|e| errno(&e), |_| 0),
                Err(e) => e,
            },
            17 => {
                let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into());
                let mut b = cwd.into_bytes();
                b.push(0);
                if b.len() as u64 > a[1] {
                    -34 // ERANGE
                } else {
                    match Self::put(bus, a[0], &b) {
                        0 => b.len() as i64,
                        e => e,
                    }
                }
            }
            34 => match self.path(bus, a[0] as i64, a[1]) {
                Ok(p) => std::fs::create_dir(&p).map_or_else(|e| errno(&e), |_| 0),
                Err(e) => e,
            },
            35 => match self.path(bus, a[0] as i64, a[1]) {
                Ok(p) => (if a[2] & 0x200 != 0 { std::fs::remove_dir(&p) } else { std::fs::remove_file(&p) }).map_or_else(|e| errno(&e), |_| 0),
                Err(e) => e,
            },
            78 => match self.path(bus, a[0] as i64, a[1]) {
                Ok(p) => {
                    let t = if p == std::path::Path::new("/proc/self/exe") { Ok(PathBuf::from(&self.exe)) } else { std::fs::read_link(&p) };
                    match t {
                        Ok(t) => {
                            let b = t.to_string_lossy().into_owned().into_bytes();
                            let n = b.len().min(a[3] as usize);
                            match Self::put(bus, a[2], &b[..n]) {
                                0 => n as i64,
                                e => e,
                            }
                        }
                        Err(e) => errno(&e),
                    }
                }
                Err(e) => e,
            },
            73 => {
                // ppoll(fds, nfds, ...): every open descriptor is always ready, closed ones are POLLNVAL
                let mut ready = 0;
                for k in 0..a[1] {
                    let p = a[0] + k * 8;
                    let Some(b) = bus.guest_bytes(p, 8) else { return Sys::Ret(EFAULT) };
                    let fd = i32::from_le_bytes(b[..4].try_into().unwrap());
                    let ev = i16::from_le_bytes(b[4..6].try_into().unwrap());
                    let open = fd >= 0 && matches!(self.fds.get(fd as usize), Some(Some(_)));
                    let rev: i16 = if open { ev & 5 } else { 0x20 };
                    if rev != 0 {
                        ready += 1;
                    }
                    bus.guest_bytes_mut(p + 6, 2).unwrap().copy_from_slice(&rev.to_le_bytes());
                }
                ready
            }
            291 => {
                // statx(dirfd, path, flags, mask, buf): built from the stat result
                let (dirfd, flags, out) = (a[0] as i64, a[2], a[4]);
                let empty = Self::cstr(bus, a[1]).is_some_and(|s| s.is_empty()) && flags & 0x1000 != 0;
                let r = if empty {
                    self.sys_fstat(bus, dirfd as usize, out)
                } else {
                    match self.path(bus, dirfd, a[1]) {
                        Ok(p) => {
                            let md = if flags & 0x100 != 0 { std::fs::symlink_metadata(&p) } else { std::fs::metadata(&p) };
                            md.map_or_else(|e| errno(&e), |m| self.stat(bus, &m, out))
                        }
                        Err(e) => e,
                    }
                };
                if r != 0 {
                    return Sys::Ret(r);
                }
                let st = bus.guest_bytes(out, 128).unwrap().to_vec();
                let g = |o: usize, n: usize| st[o..o + n].to_vec();
                let mut x = vec![0u8; 256];
                x[0..4].copy_from_slice(&0x7ffu32.to_le_bytes()); // STATX_BASIC_STATS
                x[4..8].copy_from_slice(&g(56, 4));
                x[16..20].copy_from_slice(&g(20, 4));
                x[20..24].copy_from_slice(&g(24, 4));
                x[24..28].copy_from_slice(&g(28, 4));
                x[28..30].copy_from_slice(&g(16, 2));
                x[32..40].copy_from_slice(&g(8, 8));
                x[40..48].copy_from_slice(&g(48, 8));
                x[48..56].copy_from_slice(&g(64, 8));
                for (dst, src) in [(64, 72), (96, 104), (112, 88)] {
                    x[dst..dst + 8].copy_from_slice(&g(src, 8));
                    x[dst + 8..dst + 12].copy_from_slice(&g(src + 8, 4));
                }
                Self::put(bus, out, &x)
            }
            38 | 276 => {
                // renameat(olddir, old, newdir, new) / renameat2(..., flags)
                match (self.path(bus, a[0] as i64, a[1]), self.path(bus, a[2] as i64, a[3])) {
                    (Ok(o), Ok(n)) => std::fs::rename(o, n).map_or_else(|e| errno(&e), |_| 0),
                    (Err(e), _) | (_, Err(e)) => e,
                }
            }
            36 => match (Self::cstr(bus, a[0]), self.path(bus, a[1] as i64, a[2])) {
                (Some(target), Ok(link)) => std::os::unix::fs::symlink(target, link).map_or_else(|e| errno(&e), |_| 0),
                (None, _) => EFAULT,
                (_, Err(e)) => e,
            },
            37 => match (self.path(bus, a[0] as i64, a[1]), self.path(bus, a[2] as i64, a[3])) {
                (Ok(o), Ok(n)) => std::fs::hard_link(o, n).map_or_else(|e| errno(&e), |_| 0),
                (Err(e), _) | (_, Err(e)) => e,
            },
            49 => match self.path(bus, -100, a[0]) {
                Ok(p) => std::env::set_current_dir(p).map_or_else(|e| errno(&e), |_| 0),
                Err(e) => e,
            },
            50 => match self.fds.get(a[0] as usize) {
                Some(Some(Fd::Dir(p, _, _))) => std::env::set_current_dir(p).map_or_else(|e| errno(&e), |_| 0),
                Some(Some(_)) => -20, // ENOTDIR
                _ => EBADF,
            },
            45 => match self.path(bus, -100, a[0]) {
                Ok(p) => OpenOptions::new().write(true).open(p).and_then(|f| f.set_len(a[1])).map_or_else(|e| errno(&e), |_| 0),
                Err(e) => e,
            },
            46 => match self.fds.get(a[0] as usize) {
                Some(Some(Fd::File(f))) => f.set_len(a[1]).map_or_else(|e| errno(&e), |_| 0),
                _ => EBADF,
            },
            82 | 83 => match self.fds.get(a[0] as usize) {
                Some(Some(Fd::File(f))) => f.sync_all().map_or_else(|e| errno(&e), |_| 0),
                Some(Some(_)) => 0,
                _ => EBADF,
            },
            52 | 53 | 88 | 32 | 47 | 90 | 91 | 55 | 54 | 117 | 118 | 119 | 144 | 146 | 147 | 157 | 154 | 151 | 152 => 0, // chmod/utimes/flock/fallocate/id and session calls: accepted, no effect
            155 | 158 => 0, // getpgid / getgroups
            123 => {
                // sched_getaffinity: one CPU
                let n = a[1].min(8) as usize;
                if n == 0 {
                    EINVAL
                } else {
                    match Self::put(bus, a[2], &[1, 0, 0, 0, 0, 0, 0, 0][..n]) {
                        0 => n as i64,
                        e => e,
                    }
                }
            }
            179 => {
                // sysinfo: 1 GiB total RAM, 1 process
                let mut b = vec![0u8; 112];
                b[0..8].copy_from_slice(&self.start.elapsed().as_secs().to_le_bytes());
                b[32..40].copy_from_slice(&(1u64 << 30).to_le_bytes());
                b[40..48].copy_from_slice(&(512u64 << 20).to_le_bytes());
                b[80..82].copy_from_slice(&1u16.to_le_bytes());
                b[104..108].copy_from_slice(&1u32.to_le_bytes());
                Self::put(bus, a[0], &b)
            }
            43 | 44 => {
                // statfs / fstatfs: a generic ext4-like answer
                let mut b = vec![0u8; 120];
                b[0..8].copy_from_slice(&0xef53u64.to_le_bytes());
                b[8..16].copy_from_slice(&4096u64.to_le_bytes());
                for o in [16, 24, 32] {
                    b[o..o + 8].copy_from_slice(&(1u64 << 20).to_le_bytes());
                }
                b[56..64].copy_from_slice(&255u64.to_le_bytes());
                Self::put(bus, a[1], &b)
            }
            72 => {
                // pselect6: every descriptor counts as ready; the sets are left as they are
                let n = a[0].min(1024);
                let mut ready = 0i64;
                for set in [a[1], a[2]] {
                    if set != 0 {
                        for fd in 0..n {
                            if bus.guest_bytes(set + fd / 8, 1).is_some_and(|b| b[0] >> (fd % 8) & 1 != 0) {
                                ready += 1;
                            }
                        }
                    }
                }
                ready
            }
            29 => ENOTTY, // ioctl: no terminals
            214 => {
                let want = a[0];
                if want >= self.brk_start && want < self.mmap_next.saturating_sub(PAGE) {
                    if want < self.brk {
                        if let Some(d) = bus.guest_bytes_mut(want, (self.brk - want) as usize) {
                            d.fill(0);
                        }
                    }
                    self.brk = want;
                }
                self.brk as i64
            }
            222 => self.sys_mmap(bus, a),
            215 => {
                let (addr, len) = (a[0], align_up(a[1]));
                if addr % PAGE != 0 {
                    EINVAL
                } else {
                    self.release(addr, len);
                    0
                }
            }
            216 => {
                // mremap(old, old_len, new_len, flags): move to a fresh region
                let (old, olen, nlen) = (a[0], align_up(a[1]), align_up(a[2]));
                if nlen <= olen {
                    old as i64
                } else if a[3] & 1 == 0 {
                    ENOMEM
                } else {
                    match self.alloc(bus, nlen) {
                        Some(n) => {
                            let data = bus.guest_bytes(old, olen as usize).map(|d| d.to_vec());
                            match data {
                                Some(d) => {
                                    bus.guest_bytes_mut(n, d.len()).unwrap().copy_from_slice(&d);
                                    self.release(old, olen);
                                    n as i64
                                }
                                None => EFAULT,
                            }
                        }
                        None => ENOMEM,
                    }
                }
            }
            226 | 233 | 135 | 134 | 132 | 99 | 100 | 167 => 0, // mprotect madvise sigprocmask sigaction sigaltstack robust_list ...
            96 => std::process::id() as i64, // set_tid_address -> tid
            98 => {
                // futex: single thread, so a wait could never be woken
                match a[1] & 0x7f {
                    0 | 9 => -11, // WAIT -> EAGAIN
                    _ => 0,
                }
            }
            113 => {
                let (s, ns) = if a[0] == 0 {
                    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
                    (d.as_secs(), d.subsec_nanos())
                } else {
                    let d = self.start.elapsed();
                    (d.as_secs() + 1000, d.subsec_nanos())
                };
                let mut b = s.to_le_bytes().to_vec();
                b.extend_from_slice(&(ns as u64).to_le_bytes());
                Self::put(bus, a[1], &b)
            }
            169 => {
                let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
                let mut b = d.as_secs().to_le_bytes().to_vec();
                b.extend_from_slice(&(d.subsec_micros() as u64).to_le_bytes());
                if a[0] != 0 { Self::put(bus, a[0], &b) } else { 0 }
            }
            101 | 115 => {
                // nanosleep(req, rem) / clock_nanosleep(clk, flags, req, rem)
                let req = if nr == 101 { a[0] } else { a[2] };
                match bus.guest_bytes(req, 16) {
                    Some(b) => {
                        let s = u64::from_le_bytes(b[..8].try_into().unwrap());
                        let ns = u64::from_le_bytes(b[8..].try_into().unwrap());
                        if !(nr == 115 && a[1] & 1 != 0) {
                            std::thread::sleep(std::time::Duration::new(s, ns.min(999_999_999) as u32));
                        }
                        0
                    }
                    None => EFAULT,
                }
            }
            124 => 0,
            160 => {
                let mut b = vec![0u8; 65 * 6];
                let machine = if self.arch == Arch::Riscv64 { "riscv64" } else { "aarch64" };
                for (i, s) in ["Linux", "ruvm", "6.1.0-ruvm", "#1 ruvm", machine, "(none)"].iter().enumerate() {
                    b[i * 65..i * 65 + s.len()].copy_from_slice(s.as_bytes());
                }
                Self::put(bus, a[0], &b)
            }
            166 => 0o22,
            172 | 178 => std::process::id() as i64,
            173 => std::os::unix::process::parent_id() as i64,
            174 | 175 | 176 | 177 => 1000,
            261 => {
                // prlimit64(pid, resource, new, old)
                if a[3] != 0 {
                    let cur = if a[1] == 3 { STACK_SIZE } else { u64::MAX };
                    let mut b = cur.to_le_bytes().to_vec();
                    b.extend_from_slice(&u64::MAX.to_le_bytes());
                    Self::put(bus, a[3], &b)
                } else {
                    0
                }
            }
            278 => {
                let n = a[1].min(1 << 20) as usize;
                let mut v = Vec::with_capacity(n + 8);
                while v.len() < n {
                    v.extend_from_slice(&self.rand().to_le_bytes());
                }
                v.truncate(n);
                match Self::put(bus, a[0], &v) {
                    0 => n as i64,
                    e => e,
                }
            }
            129 | 130 | 131 => {
                // kill / tkill / tgkill: a signal to ourselves ends the program the way a shell reports it;
                // a signal to a child process is passed on to the host
                let (pid, sig) = if nr == 131 { (a[1] as i32, a[2] as i32) } else { (a[0] as i32, a[1] as i32) };
                let host_sig = match sig {
                    10 => 30,
                    12 => 31,
                    17 => 20,
                    18 => 19,
                    19 => 17,
                    s => s,
                };
                if sig == 0 {
                    0
                } else if pid == std::process::id() as i32 || nr != 129 {
                    return Sys::Exit(128 + sig);
                } else if unsafe { kill(pid, host_sig) } == 0 {
                    0
                } else {
                    errno(&std::io::Error::last_os_error())
                }
            }
            220 => {
                // clone: fork semantics via a host fork (threads are not supported)
                let flags = a[0];
                if flags & 0x10000 != 0 {
                    ENOSYS
                } else {
                    let pid = unsafe { fork() };
                    if pid < 0 {
                        errno(&std::io::Error::last_os_error())
                    } else if pid == 0 {
                        let me = std::process::id();
                        if flags & 0x0100_0000 != 0 && a[4] != 0 {
                            Self::put(bus, a[4], &me.to_le_bytes()); // CLONE_CHILD_SETTID
                        }
                        return if a[1] != 0 { Sys::RetSp(0, a[1]) } else { Sys::Ret(0) };
                    } else {
                        if flags & 0x10_0000 != 0 && a[2] != 0 {
                            Self::put(bus, a[2], &(pid as u32).to_le_bytes()); // CLONE_PARENT_SETTID
                        }
                        pid as i64
                    }
                }
            }
            260 => {
                // wait4(pid, status, options, rusage)
                let mut st = 0i32;
                let pid = if (a[0] as i32) > 0 { a[0] as i32 } else { -1 };
                let r = unsafe { waitpid(pid, &mut st, a[2] as i32 & 1) };
                if r < 0 {
                    errno(&std::io::Error::last_os_error())
                } else {
                    if r > 0 && a[1] != 0 {
                        Self::put(bus, a[1], &st.to_le_bytes());
                    }
                    r as i64
                }
            }
            221 => {
                let r = self.sys_execve(bus, a);
                if r == 0 {
                    return Sys::Exec;
                }
                r
            }
            435 => ENOSYS, // clone3 (libc falls back to clone)
            _ => ENOSYS,
        };
        Sys::Ret(r)
    }

    fn strv(bus: &Bus, mut p: u64) -> Option<Vec<String>> {
        let mut v = Vec::new();
        if p == 0 {
            return Some(v);
        }
        loop {
            let ptr = u64::from_le_bytes(bus.guest_bytes(p, 8)?.try_into().unwrap());
            if ptr == 0 {
                return Some(v);
            }
            v.push(Self::cstr(bus, ptr)?);
            p += 8;
            if v.len() > 4096 {
                return None;
            }
        }
    }

    /// execve(path, argv, envp): build the new image now (so errors can be reported to the caller);
    /// the machine switches to it after the call returns. Scripts with `#!` are run through their interpreter.
    fn sys_execve(&mut self, bus: &mut Bus, a: [u64; 6]) -> i64 {
        let (Some(mut argv), Some(envp)) = (Self::strv(bus, a[1]), Self::strv(bus, a[2])) else { return EFAULT };
        let mut file = match self.path(bus, -100, a[0]) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let mut data = match std::fs::read(&file) {
            Ok(d) => d,
            Err(e) => return errno(&e),
        };
        if data.starts_with(b"#!") {
            let line = String::from_utf8_lossy(data.split(|&b| b == b'\n').next().unwrap_or(&[])[2..].to_vec().as_slice()).trim().to_string();
            let mut parts = line.splitn(2, char::is_whitespace);
            let interp = parts.next().unwrap_or("").to_string();
            let mut new_argv = vec![interp.clone()];
            new_argv.extend(parts.next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
            new_argv.push(file.to_string_lossy().into_owned());
            new_argv.extend(argv.drain(1.min(argv.len())..));
            argv = new_argv;
            file = PathBuf::from(interp);
            data = match std::fs::read(&file) {
                Ok(d) => d,
                Err(e) => return errno(&e),
            };
        }
        let mut nb = Bus::with_cpus(bus.ram_base(), bus.ram_size() as usize, 1);
        // a file that is not a guest executable (e.g. a host binary found on PATH) does not exist as far as the guest is concerned
        let Ok(img) = load(&mut nb, self.arch, &file.to_string_lossy(), &data, &argv, &envp) else { return ENOENT };
        let nu = nb.user.as_mut().unwrap();
        let keep: HashSet<usize> = (0..self.fds.len()).filter(|i| !self.cloexec.contains(i)).collect();
        nu.fds = std::mem::take(&mut self.fds).into_iter().enumerate().map(|(i, f)| f.filter(|_| keep.contains(&i))).collect();
        self.pending_exec = Some(ExecReq { bus: nb, entry: img.entry, sp: img.sp });
        0
    }

    fn sys_fstat(&mut self, bus: &mut Bus, fd: usize, out: u64) -> i64 {
        match self.fds.get(fd) {
            Some(Some(Fd::Stdin | Fd::Stdout | Fd::Stderr)) => self.stat_tty(bus, out),
            Some(Some(Fd::File(f))) => match f.metadata() {
                Ok(m) => self.stat(bus, &m, out),
                Err(e) => errno(&e),
            },
            Some(Some(Fd::Dir(p, _, _))) => match std::fs::metadata(p) {
                Ok(m) => self.stat(bus, &m, out),
                Err(e) => errno(&e),
            },
            _ => EBADF,
        }
    }
}

/// Program image placed in guest memory plus the initial stack.
pub struct UserImage {
    pub entry: u64,
    pub sp: u64,
}

/// Load a static ELF into the bus's RAM (which starts at address 0), build the Linux process
/// stack (argc, argv, envp, auxv) and install the system-call emulation.
pub fn load(bus: &mut Bus, arch: Arch, exe: &str, d: &[u8], argv: &[String], envp: &[String]) -> Result<UserImage, String> {
    let u16_at = |o: usize| d.get(o..o + 2).map_or(0, |b| u16::from_le_bytes(b.try_into().unwrap()));
    let u32_at = |o: usize| d.get(o..o + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
    let u64_at = |o: usize| d.get(o..o + 8).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()));
    if d.len() < 64 || &d[..4] != b"\x7fELF" || d[4] != 2 || d[5] != 1 {
        return Err("not a little-endian ELF64 file".into());
    }
    let want = if arch == Arch::Riscv64 { 243 } else { 183 };
    if u16_at(18) != want {
        return Err(format!("ELF e_machine {} does not match {:?}", u16_at(18), arch));
    }
    let bias: u64 = if u16_at(16) == 3 { 0x1000_0000 } else { 0 }; // ET_DYN (static-pie) is placed here
    let (phoff, phentsize, phnum) = (u64_at(32) as usize, u16_at(54) as usize, u16_at(56) as usize);
    let (mut top, mut phdr) = (0u64, 0u64);
    for k in 0..phnum {
        let p = phoff + k * phentsize;
        match u32_at(p) {
            3 => return Err("dynamically linked executables are not supported (build with -static)".into()),
            1 => {
                let (off, vaddr, filesz, memsz) = (u64_at(p + 8) as usize, u64_at(p + 16) + bias, u64_at(p + 32) as usize, u64_at(p + 40));
                if off.checked_add(filesz).map_or(true, |e| e > d.len()) {
                    return Err("truncated ELF segment".into());
                }
                bus.load(vaddr, &d[off..off + filesz])?;
                top = top.max(vaddr + memsz);
                if off <= phoff && phoff < off + filesz {
                    phdr = vaddr + (phoff - off) as u64;
                }
            }
            _ => {}
        }
    }
    let ram_end = bus.ram_base() + bus.ram_size();
    if top + STACK_SIZE + STACK_GAP + (16 << 20) > ram_end {
        return Err("program does not fit in RAM (use -m)".into());
    }
    let mut sp = ram_end - 64;
    let mut push_str = |bus: &mut Bus, s: &str| -> u64 {
        sp -= s.len() as u64 + 1;
        let mut b = s.as_bytes().to_vec();
        b.push(0);
        bus.guest_bytes_mut(sp, b.len()).unwrap().copy_from_slice(&b);
        sp
    };
    let exe_ptr = push_str(bus, exe);
    let envs: Vec<u64> = envp.iter().map(|s| push_str(bus, s)).collect();
    let args: Vec<u64> = argv.iter().map(|s| push_str(bus, s)).collect();
    sp &= !15;
    let mut rnd = [0u8; 16];
    for (i, b) in rnd.iter_mut().enumerate() {
        *b = (0x9e37_79b9u64.wrapping_mul(i as u64 + 1) >> 7) as u8 ^ 0x5a;
    }
    sp -= 16;
    bus.guest_bytes_mut(sp, 16).unwrap().copy_from_slice(&rnd);
    let rnd_ptr = sp;
    let hwcap: u64 = if arch == Arch::Riscv64 { 0x112d } else { 0x1fb };
    let auxv: [(u64, u64); 14] = [
        (3, phdr),
        (4, phentsize as u64),
        (5, phnum as u64),
        (6, PAGE),
        (7, 0),
        (8, 0),
        (9, u64_at(24) + bias),
        (11, 1000),
        (12, 1000),
        (13, 1000),
        (14, 1000),
        (16, hwcap),
        (17, 100),
        (23, 0),
    ];
    let extra = [(25, rnd_ptr), (31, exe_ptr), (0, 0)];
    let words = 1 + args.len() + 1 + envs.len() + 1 + 2 * (auxv.len() + extra.len());
    sp -= (words as u64) * 8;
    sp &= !15;
    let mut w = Vec::with_capacity(words * 8);
    let mut push = |v: u64| w.extend_from_slice(&v.to_le_bytes());
    push(args.len() as u64);
    args.iter().for_each(|&v| push(v));
    push(0);
    envs.iter().for_each(|&v| push(v));
    push(0);
    for (k, v) in auxv.iter().chain(extra.iter()) {
        push(*k);
        push(*v);
    }
    bus.guest_bytes_mut(sp, w.len()).unwrap().copy_from_slice(&w);
    let stack_low = ram_end - STACK_SIZE - STACK_GAP;
    let brk = align_up(top);
    bus.user = Some(Box::new(LinuxUser::new(arch, exe, brk, stack_low)));
    Ok(UserImage { entry: u64_at(24) + bias, sp })
}
