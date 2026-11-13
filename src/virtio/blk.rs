//! virtio-blk back-end: a raw disk image held in memory, optionally written through to
//! its backing file. The image is shared (`Disk` is cheap to clone) so it survives a
//! machine reset.

use super::{Backend, Desc, Done};
use crate::bus::Dma;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

const SECTOR: u64 = 512;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;
const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

struct DiskInner {
    data: Vec<u8>,
    file: Option<File>, // write-through backing file
}

#[derive(Clone)]
pub struct Disk(Arc<Mutex<DiskInner>>);

impl Disk {
    /// Open a raw image; guest writes go straight back to the file.
    pub fn open(path: &str) -> Result<Disk, String> {
        let mut f = File::options().read(true).write(true).open(path).map_err(|e| format!("{path}: {e}"))?;
        let mut data = Vec::new();
        f.read_to_end(&mut data).map_err(|e| format!("{path}: {e}"))?;
        data.resize(data.len().div_ceil(SECTOR as usize) * SECTOR as usize, 0);
        Ok(Disk(Arc::new(Mutex::new(DiskInner { data, file: Some(f) }))))
    }

    /// An in-memory disk (nothing is written to any file).
    pub fn from_image(mut data: Vec<u8>) -> Disk {
        data.resize(data.len().div_ceil(SECTOR as usize) * SECTOR as usize, 0);
        Disk(Arc::new(Mutex::new(DiskInner { data, file: None })))
    }

    pub fn read_file(path: &str) -> Result<Disk, String> {
        Ok(Self::from_image(std::fs::read(path).map_err(|e| format!("{path}: {e}"))?))
    }

    pub fn len(&self) -> u64 {
        self.0.lock().unwrap().data.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A copy of the current contents.
    pub fn contents(&self) -> Vec<u8> {
        self.0.lock().unwrap().data.clone()
    }
}

pub struct VirtioBlk {
    disk: Disk,
}

impl VirtioBlk {
    pub fn new(disk: Disk) -> Self {
        VirtioBlk { disk }
    }
}

impl Backend for VirtioBlk {
    fn device_id(&self) -> u32 {
        2
    }
    fn features(&self) -> u64 {
        VIRTIO_BLK_F_FLUSH
    }
    fn config(&self) -> Vec<u8> {
        (self.disk.len() / SECTOR).to_le_bytes().to_vec()
    }

    fn process(&mut self, _q: usize, mem: &mut Dma, chain: &[Desc]) -> Done {
        if chain.len() < 2 {
            return Done::Used(0);
        }
        let hdr = chain[0].addr;
        let (Some(typ), Some(sector)) = (mem.r32(hdr), mem.r64(hdr + 8)) else { return Done::Used(0) };
        let status_addr = chain[chain.len() - 1].addr;
        let data = &chain[1..chain.len() - 1];
        let mut inner = self.disk.0.lock().unwrap();
        let (mut pos, mut written, mut st) = (sector * SECTOR, 1u32, S_OK);
        match typ {
            T_IN | T_OUT => {
                for d in data {
                    let end = pos + d.len as u64;
                    if end > inner.data.len() as u64 {
                        st = S_IOERR;
                        break;
                    }
                    let range = pos as usize..end as usize;
                    if typ == T_IN {
                        if mem.write(d.addr, &inner.data[range]).is_none() {
                            st = S_IOERR;
                            break;
                        }
                        written += d.len;
                    } else {
                        let Some(buf) = mem.read(d.addr, d.len as usize) else {
                            st = S_IOERR;
                            break;
                        };
                        inner.data[range].copy_from_slice(buf);
                        if let Some(f) = &mut inner.file {
                            if f.seek(SeekFrom::Start(pos)).and_then(|_| f.write_all(buf)).is_err() {
                                st = S_IOERR;
                            }
                        }
                    }
                    pos = end;
                }
            }
            T_FLUSH => {
                if let Some(f) = &mut inner.file {
                    let _ = f.flush();
                }
            }
            T_GET_ID => {
                if let Some(d) = data.first() {
                    let id = *b"ruvm-virtio-blk\0\0\0\0\0";
                    let n = (d.len as usize).min(id.len());
                    let _ = mem.write(d.addr, &id[..n]);
                    written += n as u32;
                }
            }
            _ => st = S_UNSUPP,
        }
        let _ = mem.write(status_addr, &[st]);
        Done::Used(written)
    }
}
