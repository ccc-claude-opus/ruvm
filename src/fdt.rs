//! Minimal flattened-device-tree (DTB) writer, for describing the board to Linux
//! and firmware the way QEMU does.
//!
//! ```ignore
//! let mut t = Fdt::new();
//! t.begin("");
//! t.u32("#address-cells", 2);
//! t.begin("memory@80000000");
//! t.str("device_type", "memory");
//! t.u64s("reg", &[0x8000_0000, 128 << 20]);
//! t.end();
//! t.end();
//! let blob = t.finish();
//! ```

const MAGIC: u32 = 0xd00d_feed;
const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const END: u32 = 9;

#[derive(Default)]
pub struct Fdt {
    structs: Vec<u8>,
    strings: Vec<u8>,
    depth: usize,
}

impl Fdt {
    pub fn new() -> Self {
        Self::default()
    }

    fn word(&mut self, v: u32) {
        self.structs.extend_from_slice(&v.to_be_bytes());
    }

    fn pad(&mut self) {
        while self.structs.len() % 4 != 0 {
            self.structs.push(0);
        }
    }

    fn name_off(&mut self, name: &str) -> u32 {
        // reuse an existing string if possible
        let needle: Vec<u8> = name.bytes().chain(std::iter::once(0)).collect();
        if let Some(p) = self.strings.windows(needle.len()).position(|w| w == needle.as_slice()) {
            if p == 0 || self.strings[p - 1] == 0 {
                return p as u32;
            }
        }
        let off = self.strings.len() as u32;
        self.strings.extend_from_slice(&needle);
        off
    }

    pub fn begin(&mut self, name: &str) {
        self.word(BEGIN_NODE);
        self.structs.extend_from_slice(name.as_bytes());
        self.structs.push(0);
        self.pad();
        self.depth += 1;
    }

    pub fn end(&mut self) {
        self.word(END_NODE);
        self.depth -= 1;
    }

    pub fn prop(&mut self, name: &str, value: &[u8]) {
        let off = self.name_off(name);
        self.word(PROP);
        self.word(value.len() as u32);
        self.word(off);
        self.structs.extend_from_slice(value);
        self.pad();
    }

    pub fn empty(&mut self, name: &str) {
        self.prop(name, &[]);
    }

    pub fn u32(&mut self, name: &str, v: u32) {
        self.u32s(name, &[v]);
    }

    pub fn u32s(&mut self, name: &str, v: &[u32]) {
        let b: Vec<u8> = v.iter().flat_map(|x| x.to_be_bytes()).collect();
        self.prop(name, &b);
    }

    pub fn u64(&mut self, name: &str, v: u64) {
        self.u64s(name, &[v]);
    }

    /// 64-bit cells (e.g. `reg` with #address-cells = #size-cells = 2).
    pub fn u64s(&mut self, name: &str, v: &[u64]) {
        let b: Vec<u8> = v.iter().flat_map(|x| x.to_be_bytes()).collect();
        self.prop(name, &b);
    }

    pub fn str(&mut self, name: &str, s: &str) {
        self.strs(name, &[s]);
    }

    /// A string list (e.g. `compatible = "a", "b"`).
    pub fn strs(&mut self, name: &str, list: &[&str]) {
        let mut b = Vec::new();
        for s in list {
            b.extend_from_slice(s.as_bytes());
            b.push(0);
        }
        self.prop(name, &b);
    }

    /// Produce the DTB blob (version 17).
    pub fn finish(mut self) -> Vec<u8> {
        assert_eq!(self.depth, 0, "unbalanced FDT nodes");
        self.word(END);
        let header = 40u32;
        let rsvmap = 16u32; // one terminating (0, 0) entry
        let off_struct = header + rsvmap;
        let off_strings = off_struct + self.structs.len() as u32;
        let total = off_strings + self.strings.len() as u32;
        let mut out = Vec::with_capacity(total as usize);
        for v in [
            MAGIC,
            total,
            off_struct,
            off_strings,
            header, // off_mem_rsvmap
            17,     // version
            16,     // last compatible version
            0,      // boot cpu
            self.strings.len() as u32,
            self.structs.len() as u32,
        ] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&[0; 16]);
        out.extend_from_slice(&self.structs);
        out.extend_from_slice(&self.strings);
        out
    }
}
