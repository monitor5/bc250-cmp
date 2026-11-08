//! Fixed-size ring buffers for history + an on-disk ring file (pwrite of one
//! 20-byte record per minute, so disk wear and RAM are both tiny).
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::Path;

pub const NONE8: u8 = 255;
pub const NONE16: u16 = u16::MAX;
const REC: usize = 20;
const HDR: usize = 20;
const MAGIC: &[u8; 4] = b"B25H";

/// One history point. Percent values are 0..100, memory values are per-mille
/// of total (0..1000), temps in °C, power in 0.1 W. 255 / 65535 = missing.
#[derive(Clone, Copy, Default)]
pub struct Pt {
    pub ts: u32,
    pub cpu: u8,
    pub cpu_max: u8,
    pub gpu: u8,
    pub gpu_max: u8,
    pub mem: u16,
    pub vram: u16,
    pub gtt: u16,
    pub gtemp: u8,
    pub ctemp: u8,
    pub pwr: u16,
    pub swap: u16,
}

impl Pt {
    fn enc(&self) -> [u8; REC] {
        let mut b = [0u8; REC];
        b[0..4].copy_from_slice(&self.ts.to_le_bytes());
        b[4] = self.cpu;
        b[5] = self.cpu_max;
        b[6] = self.gpu;
        b[7] = self.gpu_max;
        b[8..10].copy_from_slice(&self.mem.to_le_bytes());
        b[10..12].copy_from_slice(&self.vram.to_le_bytes());
        b[12..14].copy_from_slice(&self.gtt.to_le_bytes());
        b[14] = self.gtemp;
        b[15] = self.ctemp;
        b[16..18].copy_from_slice(&self.pwr.to_le_bytes());
        b[18..20].copy_from_slice(&self.swap.to_le_bytes());
        b
    }
    fn dec(b: &[u8]) -> Pt {
        let u16a = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        Pt {
            ts: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            cpu: b[4],
            cpu_max: b[5],
            gpu: b[6],
            gpu_max: b[7],
            mem: u16a(8),
            vram: u16a(10),
            gtt: u16a(12),
            gtemp: b[14],
            ctemp: b[15],
            pwr: u16a(16),
            swap: u16a(18),
        }
    }
    /// Values as (avg-type fields, max-type fields) for bucketing.
    /// Order: cpu, cpu_max, gpu, gpu_max, mem, vram, gtt, gtemp, ctemp, pwr
    fn vals(&self) -> [Option<f32>; NF] {
        let o8 = |v: u8| if v == NONE8 { None } else { Some(v as f32) };
        let o16 = |v: u16, d: f32| if v == NONE16 { None } else { Some(v as f32 / d) };
        [
            o8(self.cpu),
            o8(self.cpu_max),
            o8(self.gpu),
            o8(self.gpu_max),
            o16(self.mem, 10.0),
            o16(self.vram, 10.0),
            o16(self.gtt, 10.0),
            o8(self.gtemp),
            o8(self.ctemp),
            o16(self.pwr, 10.0),
            o16(self.swap, 10.0),
        ]
    }
}

const NF: usize = 11;
const NAMES: [&str; NF] = ["cpu", "cpuMax", "gpu", "gpuMax", "mem", "vram", "gtt", "gtemp", "ctemp", "pwr", "swap"];
const IS_MAX: [bool; NF] = [false, true, false, true, false, false, false, false, false, false, false];

pub struct Ring {
    buf: Vec<Pt>,
    head: usize, // next write index
    len: usize,
}

impl Ring {
    pub fn new(cap: usize) -> Ring {
        Ring { buf: vec![Pt::default(); cap.max(1)], head: 0, len: 0 }
    }
    pub fn cap(&self) -> usize {
        self.buf.len()
    }
    pub fn push(&mut self, p: Pt) -> usize {
        let i = self.head;
        self.buf[i] = p;
        self.head = (i + 1) % self.cap();
        if self.len < self.cap() {
            self.len += 1;
        }
        i
    }
    pub fn iter(&self) -> impl Iterator<Item = &Pt> {
        let cap = self.cap();
        let start = (self.head + cap - self.len) % cap;
        (0..self.len).map(move |k| &self.buf[(start + k) % cap])
    }
}

/// Ring persisted to a fixed-size file.
pub struct Store {
    pub ring: Ring,
    file: Option<File>,
}

impl Store {
    pub fn open(path: Option<&Path>, cap: usize) -> Store {
        let mut ring = Ring::new(cap);
        let Some(path) = path else { return Store { ring, file: None } };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Load old contents (any capacity) chronologically.
        let mut reuse = false;
        if let Ok(f) = File::open(path) {
            let mut data = Vec::new();
            // Bound startup memory, and preserve an unexpectedly large file for inspection.
            const MAX_FILE: u64 = 20 + 30 * 1440 * 20;
            if f.take(MAX_FILE + 1).read_to_end(&mut data).is_err() || data.len() as u64 > MAX_FILE {
                eprintln!("bc250mon: history file is unreadable or too large; persistence disabled");
                return Store { ring, file: None };
            }
            if data.len() >= HDR && &data[0..4] == MAGIC
                && u16::from_le_bytes([data[6], data[7]]) as usize == REC
            {
                let ocap = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
                let ohead = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
                let olen = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;
                if ocap > 0 && data.len() >= HDR + ocap * REC && ohead < ocap && olen <= ocap {
                    let start = (ohead + ocap - olen) % ocap;
                    for k in 0..olen {
                        let i = (start + k) % ocap;
                        ring.push(Pt::dec(&data[HDR + i * REC..HDR + (i + 1) * REC]));
                    }
                    reuse = ocap == cap;
                }
            }
        }
        let file = OpenOptions::new().read(true).write(true).create(true).open(path);
        let file = match file {
            Ok(f) => f,
            Err(e) => {
                eprintln!("bc250mon: cannot open {}: {e} (history will not persist)", path.display());
                return Store { ring, file: None };
            }
        };
        let s = Store { ring, file: Some(file) };
        if !reuse {
            s.rewrite();
        }
        s
    }

    fn header(&self) -> [u8; HDR] {
        let mut h = [0u8; HDR];
        h[0..4].copy_from_slice(MAGIC);
        h[4..6].copy_from_slice(&1u16.to_le_bytes());
        h[6..8].copy_from_slice(&(REC as u16).to_le_bytes());
        h[8..12].copy_from_slice(&(self.ring.cap() as u32).to_le_bytes());
        h[12..16].copy_from_slice(&(self.ring.head as u32).to_le_bytes());
        h[16..20].copy_from_slice(&(self.ring.len as u32).to_le_bytes());
        h
    }

    fn rewrite(&self) {
        let Some(f) = &self.file else { return };
        let _ = f.set_len((HDR + self.ring.cap() * REC) as u64);
        for (i, p) in self.ring.buf.iter().enumerate() {
            let _ = f.write_all_at(&p.enc(), (HDR + i * REC) as u64);
        }
        let _ = f.write_all_at(&self.header(), 0);
    }

    pub fn push(&mut self, p: Pt) {
        let i = self.ring.push(p);
        if let Some(f) = &self.file {
            let _ = f.write_all_at(&p.enc(), (HDR + i * REC) as u64);
            let _ = f.write_all_at(&self.header(), 0);
        }
    }
}

/// Bucket points with ts >= from into at most `maxn` buckets; columnar JSON.
pub fn query<'a>(pts: impl Iterator<Item = &'a Pt>, from: u32, to: u32, native_step: u32, maxn: u32) -> String {
    let span = to.saturating_sub(from).max(1);
    let step = native_step.max((span + maxn - 1) / maxn.max(1));
    let nb = (span / step + 1) as usize;
    let mut sum = vec![[0f32; NF]; nb];
    let mut cnt = vec![[0u16; NF]; nb];
    for p in pts {
        if p.ts < from || p.ts > to {
            continue;
        }
        let b = ((p.ts - from) / step) as usize;
        if b >= nb {
            continue;
        }
        for (k, v) in p.vals().iter().enumerate() {
            if let Some(v) = v {
                if IS_MAX[k] {
                    if cnt[b][k] == 0 || *v > sum[b][k] {
                        sum[b][k] = *v;
                    }
                } else {
                    sum[b][k] += *v;
                }
                cnt[b][k] += 1;
            }
        }
    }
    let mut out = String::with_capacity(64 + nb * 40);
    out.push_str(&format!("{{\"step\":{step},\"t\":["));
    let used: Vec<usize> = (0..nb).filter(|&b| cnt[b][0] > 0 || cnt[b][4] > 0).collect();
    for (j, &b) in used.iter().enumerate() {
        if j > 0 { out.push(','); }
        out.push_str(&(from + b as u32 * step).to_string());
    }
    out.push(']');
    for k in 0..NF {
        out.push_str(&format!(",\"{}\":[", NAMES[k]));
        for (j, &b) in used.iter().enumerate() {
            if j > 0 { out.push(','); }
            if cnt[b][k] == 0 {
                out.push_str("null");
            } else {
                let v = if IS_MAX[k] { sum[b][k] } else { sum[b][k] / cnt[b][k] as f32 };
                out.push_str(&format!("{:.1}", v));
            }
        }
        out.push(']');
    }
    out.push('}');
    out
}

/// Per-minute accumulator to build coarse points from fine samples.
#[derive(Default)]
pub struct Acc {
    min: u32,
    n: [u32; NF],
    s: [f32; NF],
}

impl Acc {
    /// Returns a finished minute point when `p` starts a new minute.
    pub fn add(&mut self, p: &Pt) -> Option<Pt> {
        let m = p.ts / 60;
        let mut out = None;
        if m != self.min && self.n.iter().any(|&n| n > 0) {
            out = Some(self.finish());
        }
        if m != self.min {
            *self = Acc { min: m, ..Default::default() };
        }
        for (k, v) in p.vals().iter().enumerate() {
            if let Some(v) = v {
                if IS_MAX[k] { self.s[k] = self.s[k].max(*v) } else { self.s[k] += *v }
                self.n[k] += 1;
            }
        }
        out
    }
    fn finish(&self) -> Pt {
        let g = |k: usize| -> Option<f32> {
            if self.n[k] == 0 { None } else if IS_MAX[k] { Some(self.s[k]) } else { Some(self.s[k] / self.n[k] as f32) }
        };
        let u8v = |k| g(k).map(|v| v.round().clamp(0.0, 254.0) as u8).unwrap_or(NONE8);
        let u16v = |k, m: f32| g(k).map(|v| (v * m).round().clamp(0.0, 65534.0) as u16).unwrap_or(NONE16);
        Pt {
            ts: self.min * 60,
            cpu: u8v(0),
            cpu_max: u8v(1),
            gpu: u8v(2),
            gpu_max: u8v(3),
            mem: u16v(4, 10.0),
            vram: u16v(5, 10.0),
            gtt: u16v(6, 10.0),
            gtemp: u8v(7),
            ctemp: u8v(8),
            pwr: u16v(9, 10.0),
            swap: u16v(10, 10.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pt(ts: u32, v: u8) -> Pt {
        Pt { ts, cpu: v, cpu_max: v, gpu: v, gpu_max: v, mem: 500, vram: NONE16, gtt: 100, gtemp: 50, ctemp: NONE8, pwr: 1000, swap: 0 }
    }
    #[test]
    fn ring_wraps() {
        let mut r = Ring::new(3);
        for i in 1..=5 { r.push(pt(i, i as u8)); }
        let v: Vec<u32> = r.iter().map(|p| p.ts).collect();
        assert_eq!(v, vec![3, 4, 5]);
    }
    #[test]
    fn store_persists_and_resizes() {
        let p = std::env::temp_dir().join(format!("bc250mon-test-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&p);
        { let mut s = Store::open(Some(&p), 4); for i in 1..=6 { s.push(pt(i * 60, i as u8)); } }
        { let s = Store::open(Some(&p), 4); let v: Vec<u32> = s.ring.iter().map(|p| p.ts).collect(); assert_eq!(v, vec![180, 240, 300, 360]); }
        { let s = Store::open(Some(&p), 2); let v: Vec<u32> = s.ring.iter().map(|p| p.ts).collect(); assert_eq!(v, vec![300, 360]); }
        { let s = Store::open(Some(&p), 2); assert_eq!(s.ring.iter().count(), 2); }
        let _ = std::fs::remove_file(&p);
    }
    #[test]
    fn oversized_history_is_preserved() {
        let p = std::env::temp_dir().join(format!("bc250mon-oversized-{}.bin", std::process::id()));
        let f = File::create(&p).unwrap();
        let size = 20 + 30 * 1440 * 20 + 1;
        f.set_len(size).unwrap();
        drop(f);
        let store = Store::open(Some(&p), 4);
        assert!(store.file.is_none());
        assert_eq!(std::fs::metadata(&p).unwrap().len(), size);
        let _ = std::fs::remove_file(&p);
    }
    #[test]
    fn acc_minute() {
        let mut a = Acc::default();
        assert!(a.add(&pt(600, 10)).is_none());
        assert!(a.add(&pt(630, 30)).is_none());
        let m = a.add(&pt(660, 0)).unwrap();
        assert_eq!((m.ts, m.cpu, m.cpu_max, m.vram), (600, 20, 30, NONE16));
    }
    #[test]
    fn query_buckets() {
        let mut r = Ring::new(100);
        for i in 0..60 { r.push(pt(1000 + i, (i % 10) as u8)); }
        let j = query(r.iter(), 1000, 1059, 1, 6);
        assert!(j.starts_with("{\"step\":10,\"t\":[1000,1010,1020,1030,1040,1050]"), "{j}");
        assert!(j.contains("\"cpu\":[4.5,4.5"), "{j}");
        assert!(j.contains("\"vram\":[null"), "{j}");
    }
}
