//! Readers for /proc and amdgpu sysfs. Everything is read on demand; nothing is cached
//! except a handful of paths.
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

extern "C" {
    fn sysconf(name: i32) -> i64;
    fn statvfs(path: *const std::ffi::c_char, buf: *mut u64) -> i32;
}

const DISK_FS: &[&str] = &["ext2", "ext3", "ext4", "xfs", "btrfs", "f2fs", "vfat", "exfat", "ntfs", "ntfs3", "zfs", "bcachefs", "jfs", "reiserfs"];

/// Local filesystems: (mountpoint, device, fstype, total, used, avail) bytes, de-duplicated by device.
pub fn disks() -> Vec<(String, String, String, u64, u64, u64)> {
    let mut out: Vec<(String, String, String, u64, u64, u64)> = Vec::new();
    let s = read_str(Path::new("/proc/self/mounts")).unwrap_or_default();
    for l in s.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 3 || !DISK_FS.contains(&f[2]) || out.iter().any(|d| d.1 == f[0]) {
            continue;
        }
        let mnt = f[1].replace("\\040", " ");
        let Ok(c) = std::ffi::CString::new(mnt.as_bytes()) else { continue };
        let mut b = [0u64; 32]; // struct statvfs is < 128 bytes on x86_64 (glibc & musl)
        if unsafe { statvfs(c.as_ptr(), b.as_mut_ptr()) } != 0 {
            continue;
        }
        // f_bsize, f_frsize, f_blocks, f_bfree, f_bavail
        let fr = if b[1] > 0 { b[1] } else { b[0] };
        let (total, free, avail) = (b[2] * fr, b[3] * fr, b[4] * fr);
        if total == 0 {
            continue;
        }
        out.push((mnt, f[0].to_string(), f[2].to_string(), total, total - free, avail));
    }
    out
}

pub fn clk_tck() -> f64 {
    let v = unsafe { sysconf(2) }; // _SC_CLK_TCK
    if v > 0 { v as f64 } else { 100.0 }
}

pub fn read_str(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok()
}
pub fn read_u64(p: &Path) -> Option<u64> {
    read_str(p)?.trim().parse().ok()
}

pub fn hostname() -> String {
    read_str(Path::new("/proc/sys/kernel/hostname")).map(|s| s.trim().to_string()).unwrap_or_else(|| "node".into())
}

// ---------------------------------------------------------------- GPU (amdgpu sysfs)

pub struct Gpu {
    dev: PathBuf,
    hwmon: Option<PathBuf>,
}

impl Gpu {
    pub fn detect(over: Option<PathBuf>) -> Option<Gpu> {
        let dev = match over {
            Some(p) => p,
            None => {
                let mut found = None;
                let mut cards: Vec<PathBuf> = fs::read_dir("/sys/class/drm").ok()?.flatten()
                    .filter(|e| { let n = e.file_name(); let n = n.to_string_lossy(); n.starts_with("card") && !n.contains('-') })
                    .map(|e| e.path()).collect();
                cards.sort();
                for c in cards {
                    let d = c.join("device");
                    let amd = read_str(&d.join("vendor")).map(|v| v.trim() == "0x1002").unwrap_or(false);
                    if amd && d.join("gpu_busy_percent").exists() {
                        found = Some(d);
                        break;
                    }
                }
                found?
            }
        };
        let hwmon = fs::read_dir(dev.join("hwmon")).ok().and_then(|mut r| r.next()).and_then(|e| e.ok()).map(|e| e.path());
        Some(Gpu { dev, hwmon })
    }
    pub fn busy(&self) -> Option<u64> {
        read_u64(&self.dev.join("gpu_busy_percent"))
    }
    pub fn mem(&self, kind: &str) -> Option<(u64, u64)> {
        let t = read_u64(&self.dev.join(format!("mem_info_{kind}_total")))?;
        let u = read_u64(&self.dev.join(format!("mem_info_{kind}_used")))?;
        Some((t, u))
    }
    pub fn temp(&self) -> Option<f32> {
        read_u64(&self.hwmon.as_ref()?.join("temp1_input")).map(|v| v as f32 / 1000.0)
    }
    /// Watts
    pub fn power(&self) -> Option<f32> {
        let h = self.hwmon.as_ref()?;
        read_u64(&h.join("power1_average")).or_else(|| read_u64(&h.join("power1_input"))).map(|v| v as f32 / 1e6)
    }
    /// Current shader clock in MHz (line marked with '*' in pp_dpm_sclk)
    pub fn sclk(&self) -> Option<u32> {
        let s = read_str(&self.dev.join("pp_dpm_sclk"))?;
        let l = s.lines().find(|l| l.trim_end().ends_with('*'))?;
        let mhz = l.split_whitespace().nth(1)?;
        mhz.trim_end_matches("Mhz").trim_end_matches("MHz").parse().ok()
    }
}

/// CPU temperature (k10temp Tctl) path, if present.
pub fn cpu_temp_path() -> Option<PathBuf> {
    for e in fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let p = e.path();
        if read_str(&p.join("name")).map(|n| n.trim() == "k10temp").unwrap_or(false) {
            return Some(p.join("temp1_input"));
        }
    }
    None
}

// ---------------------------------------------------------------- CPU / MEM

/// (busy, total) jiffies for aggregate (index 0) and each core.
pub fn cpu_times() -> Vec<(u64, u64)> {
    let s = read_str(Path::new("/proc/stat")).unwrap_or_default();
    s.lines()
        .take_while(|l| l.starts_with("cpu"))
        .map(|l| {
            let v: Vec<u64> = l.split_whitespace().skip(1).take(8).filter_map(|x| x.parse().ok()).collect();
            let total: u64 = v.iter().sum();
            let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
            (total - idle, total)
        })
        .collect()
}

pub fn pct(a: (u64, u64), b: (u64, u64)) -> f32 {
    let dt = b.1.saturating_sub(a.1);
    if dt == 0 { 0.0 } else { (b.0.saturating_sub(a.0)) as f32 * 100.0 / dt as f32 }
}

#[derive(Default, Clone)]
pub struct Mem {
    pub total: u64, // kB
    pub avail: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

pub fn meminfo() -> Mem {
    let s = read_str(Path::new("/proc/meminfo")).unwrap_or_default();
    let mut m = Mem::default();
    for l in s.lines() {
        let mut it = l.split_whitespace();
        let (Some(k), Some(v)) = (it.next(), it.next()) else { continue };
        let v: u64 = v.parse().unwrap_or(0);
        match k {
            "MemTotal:" => m.total = v,
            "MemAvailable:" => m.avail = v,
            "SwapTotal:" => m.swap_total = v,
            "SwapFree:" => m.swap_free = v,
            _ => {}
        }
    }
    m
}

pub fn loadavg() -> [f32; 3] {
    let s = read_str(Path::new("/proc/loadavg")).unwrap_or_default();
    let mut r = [0f32; 3];
    for (i, x) in s.split_whitespace().take(3).enumerate() {
        r[i] = x.parse().unwrap_or(0.0);
    }
    r
}

pub fn uptime() -> u64 {
    read_str(Path::new("/proc/uptime")).and_then(|s| s.split('.').next().and_then(|x| x.parse().ok())).unwrap_or(0)
}

pub fn self_rss_kb() -> u64 {
    read_str(Path::new("/proc/self/statm"))
        .and_then(|s| s.split_whitespace().nth(1).and_then(|x| x.parse::<u64>().ok()))
        .map(|p| p * 4)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- Processes

struct PStat {
    comm: String,
    state: char,
    ticks: u64,
    thr: u32,
    rss_pages: u64,
}

fn read_pstat(pid: u32) -> Option<PStat> {
    let s = read_str(Path::new(&format!("/proc/{pid}/stat")))?;
    let l = s.find('(')?;
    let r = s.rfind(')')?;
    let comm = s[l + 1..r].to_string();
    let f: Vec<&str> = s[r + 2..].split_whitespace().collect();
    // f[0]=state(3) f[11]=utime(14) f[12]=stime(15) f[17]=threads(20) f[21]=rss(24)
    Some(PStat {
        comm,
        state: f.first()?.chars().next()?,
        ticks: f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?,
        thr: f.get(17)?.parse().ok()?,
        rss_pages: f.get(21)?.parse().ok()?,
    })
}

#[derive(Default, Clone, Copy)]
struct DrmInfo {
    client: u64,
    engine_ns: u64,
    has_engine: bool,
    vram_kib: u64,
    gtt_kib: u64,
}

fn parse_kib(v: &str) -> u64 {
    let mut it = v.split_whitespace();
    let n: u64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    match it.next() {
        Some("KiB") => n,
        Some("MiB") => n * 1024,
        Some("GiB") => n * 1024 * 1024,
        _ => n / 1024,
    }
}

fn read_fdinfo(pid: u32, fd: &str) -> Option<DrmInfo> {
    let s = read_str(Path::new(&format!("/proc/{pid}/fdinfo/{fd}")))?;
    let mut d = DrmInfo::default();
    let (mut mem_v, mut mem_g, mut res_v, mut res_g) = (None, None, None, None);
    let mut is_drm = false;
    for l in s.lines() {
        let Some((k, v)) = l.split_once(':') else { continue };
        let v = v.trim();
        match k {
            "drm-client-id" => { is_drm = true; d.client = v.parse().unwrap_or(0) }
            "drm-memory-vram" => mem_v = Some(parse_kib(v)),
            "drm-memory-gtt" => mem_g = Some(parse_kib(v)),
            "drm-resident-vram" => res_v = Some(parse_kib(v)),
            "drm-resident-gtt" => res_g = Some(parse_kib(v)),
            _ if k.starts_with("drm-engine-") && !k.starts_with("drm-engine-capacity") => {
                if let Some(ns) = v.split_whitespace().next().and_then(|x| x.parse::<u64>().ok()) {
                    d.engine_ns = d.engine_ns.saturating_add(ns);
                    d.has_engine = true;
                }
            }
            _ => {}
        }
    }
    if !is_drm {
        return None;
    }
    d.vram_kib = mem_v.or(res_v).unwrap_or(0);
    d.gtt_kib = mem_g.or(res_g).unwrap_or(0);
    Some(d)
}

/// Per-client engine counters for a single PCI device, as used by GPU monitors.
/// Missing counters stay missing: some drivers expose memory but no engine time.
pub type EngineSnapshot = HashMap<(u64, String), u64>;

fn parse_engine_fdinfo(text: &str, pdev: &str, out: &mut EngineSnapshot) {
    let mut client = None;
    let mut device = None;
    let mut engines = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match key {
            "drm-client-id" => client = value.parse::<u64>().ok(),
            "drm-pdev" => device = Some(value),
            "drm-engine-gfx" | "drm-engine-compute" => {
                if let Some(ns) = value.split_whitespace().next().and_then(|v| v.parse::<u64>().ok()) {
                    engines.push((key.to_owned(), ns));
                }
            }
            _ => {}
        }
    }
    if device != Some(pdev) { return; }
    if let Some(client) = client {
        for (engine, ns) in engines {
            // Shared/duplicated descriptors refer to the same DRM client.
            out.entry((client, engine)).and_modify(|n| *n = (*n).max(ns)).or_insert(ns);
        }
    }
}

pub fn gpu_engine_snapshot(gpu: &Gpu) -> EngineSnapshot {
    let mut out = EngineSnapshot::new();
    let Ok(device) = fs::canonicalize(&gpu.dev) else { return out };
    let Some(pdev) = device.file_name().and_then(|n| n.to_str()) else { return out };
    let Ok(procs) = fs::read_dir("/proc") else { return out };
    for proc in procs.flatten() {
        let Some(pid) = proc.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        for fd in drm_fds(pid) {
            if let Some(text) = read_str(Path::new(&format!("/proc/{pid}/fdinfo/{fd}"))) {
                parse_engine_fdinfo(&text, pdev, &mut out);
            }
            if out.len() >= 4096 { return out; }
        }
    }
    out
}

/// Aggregate client-engine busy time. Multiple engines can overlap, so this is
/// an estimate, not hardware-wide occupancy or per-CU utilization.
pub fn gpu_engine_percent(before: &EngineSnapshot, after: &EngineSnapshot, elapsed: f64) -> Option<f32> {
    if elapsed <= 0.0 { return None; }
    let mut any = false;
    let mut ns = 0u64;
    for (key, now) in after {
        if let Some(old) = before.get(key) {
            if now >= old { any = true; ns = ns.saturating_add(now - old); }
        }
    }
    if any { Some((ns as f64 / (elapsed * 1e9) * 100.0).min(100.0) as f32) } else { None }
}

/// Sum DRM usage over unique clients of a process, given its DRM fds.
fn drm_usage(pid: u32, fds: &[String]) -> DrmInfo {
    let mut seen: Vec<u64> = Vec::new();
    let mut tot = DrmInfo::default();
    for fd in fds {
        if let Some(d) = read_fdinfo(pid, fd) {
            if seen.contains(&d.client) { continue; }
            seen.push(d.client);
            tot.engine_ns = tot.engine_ns.saturating_add(d.engine_ns);
            tot.has_engine |= d.has_engine;
            tot.vram_kib += d.vram_kib;
            tot.gtt_kib += d.gtt_kib;
        }
    }
    tot
}

fn drm_fds(pid: u32) -> Vec<String> {
    let mut v = Vec::new();
    let Ok(rd) = fs::read_dir(format!("/proc/{pid}/fd")) else { return v };
    for e in rd.flatten() {
        if let Ok(t) = fs::read_link(e.path()) {
            if t.as_os_str().as_encoded_bytes().starts_with(b"/dev/dri/") {
                v.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    v
}

fn users() -> HashMap<u32, String> {
    let mut m = HashMap::new();
    for l in read_str(Path::new("/etc/passwd")).unwrap_or_default().lines() {
        let f: Vec<&str> = l.split(':').collect();
        if f.len() > 2 {
            if let Ok(uid) = f[2].parse() { m.insert(uid, f[0].to_string()); }
        }
    }
    m
}

pub fn esc(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Two snapshots `gap` apart -> per-process CPU%, GPU%, RSS, VRAM/GTT. Returns JSON.
pub fn procs_json(limit: usize, gap: Duration) -> String {
    struct P { pid: u32, uid: u32, a: PStat, fds: Vec<String>, d0: DrmInfo }
    let mut list: Vec<P> = Vec::new();
    let t0 = Instant::now();
    if let Ok(rd) = fs::read_dir("/proc") {
        for e in rd.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            let Some(a) = read_pstat(pid) else { continue };
            let uid = e.metadata().map(|m| m.uid()).unwrap_or(0);
            let fds = if a.rss_pages > 0 { drm_fds(pid) } else { Vec::new() };
            let d0 = if fds.is_empty() { DrmInfo::default() } else { drm_usage(pid, &fds) };
            list.push(P { pid, uid, a, fds, d0 });
        }
    }
    std::thread::sleep(gap);
    let el = t0.elapsed().as_secs_f64();
    let hz = clk_tck();
    let page_kb = 4u64;
    let names = users();

    struct R { pid: u32, uid: u32, comm: String, state: char, thr: u32, rss: u64, cpu: f64, gpu: Option<f64>, vram: u64, gtt: u64 }
    let mut rows: Vec<R> = Vec::with_capacity(list.len());
    for p in list {
        let Some(b) = read_pstat(p.pid) else { continue };
        let cpu = b.ticks.saturating_sub(p.a.ticks) as f64 / hz / el * 100.0;
        let (gpu, vram, gtt) = if p.fds.is_empty() {
            (None, 0, 0)
        } else {
            let d1 = drm_usage(p.pid, &p.fds);
            (if d1.has_engine && p.d0.has_engine { Some(d1.engine_ns.saturating_sub(p.d0.engine_ns) as f64 / (el * 1e9) * 100.0) } else { None }, d1.vram_kib, d1.gtt_kib)
        };
        rows.push(R { pid: p.pid, uid: p.uid, comm: b.comm, state: b.state, thr: b.thr, rss: b.rss_pages * page_kb, cpu, gpu, vram, gtt });
    }
    let total = rows.len();
    rows.sort_by(|x, y| {
        let kx = x.cpu + x.gpu.unwrap_or(0.0) + (x.rss as f64) * 1e-9;
        let ky = y.cpu + y.gpu.unwrap_or(0.0) + (y.rss as f64) * 1e-9;
        ky.partial_cmp(&kx).unwrap_or(std::cmp::Ordering::Equal)
    });
    // keep top `limit` by activity, but always include the biggest memory users
    if rows.len() > limit {
        let mut by_rss: Vec<usize> = (limit..rows.len()).collect();
        by_rss.sort_by(|&i, &j| (rows[j].rss + rows[j].vram + rows[j].gtt).cmp(&(rows[i].rss + rows[i].vram + rows[i].gtt)));
        let extra: Vec<usize> = by_rss.into_iter().take(limit / 5).collect();
        let mut keep = vec![false; rows.len()];
        for i in 0..limit { keep[i] = true; }
        for i in extra { keep[i] = true; }
        let mut k = keep.iter();
        rows.retain(|_| *k.next().unwrap());
    }

    let mut out = String::with_capacity(rows.len() * 160);
    out.push_str(&format!("{{\"total\":{total},\"procs\":["));
    for (i, r) in rows.iter().enumerate() {
        if i > 0 { out.push(','); }
        let mut cmd = read_str(Path::new(&format!("/proc/{}/cmdline", r.pid)))
            .map(|s| s.trim_end_matches('\0').replace('\0', " "))
            .unwrap_or_default();
        if cmd.len() > 400 {
            let mut c = 400;
            while !cmd.is_char_boundary(c) { c -= 1; }
            cmd.truncate(c);
        }
        let user = names.get(&r.uid).cloned().unwrap_or_else(|| r.uid.to_string());
        out.push_str(&format!("{{\"pid\":{},\"user\":", r.pid));
        esc(&user, &mut out);
        out.push_str(",\"name\":");
        esc(&r.comm, &mut out);
        out.push_str(",\"cmd\":");
        esc(&cmd, &mut out);
        out.push_str(&format!(
            ",\"s\":\"{}\",\"thr\":{},\"cpu\":{:.1},\"rss\":{},\"gpu\":{},\"vram\":{},\"gtt\":{}}}",
            if r.state == '"' || r.state == '\\' { '?' } else { r.state },
            r.thr, r.cpu, r.rss,
            r.gpu.map(|g| format!("{:.1}", g)).unwrap_or_else(|| "null".into()),
            r.vram, r.gtt
        ));
    }
    out.push_str("]}");
    out
}

#[cfg(test)]
mod gpu_engine_tests {
    use super::*;
    #[test]
    fn engine_counters_filter_device_and_deduplicate() {
        let mut a = EngineSnapshot::new();
        let sample = "drm-client-id: 5\ndrm-pdev: 0000:01:00.0\ndrm-engine-gfx: 100 ns\ndrm-engine-compute: 200 ns\n";
        parse_engine_fdinfo(sample, "0000:01:00.0", &mut a);
        parse_engine_fdinfo(sample, "0000:01:00.0", &mut a);
        parse_engine_fdinfo(sample, "0000:02:00.0", &mut a);
        assert_eq!(a.len(), 2);
        let mut b = a.clone();
        *b.get_mut(&(5, "drm-engine-compute".into())).unwrap() += 500_000_000;
        assert_eq!(gpu_engine_percent(&a, &b, 1.0), Some(50.0));
        assert_eq!(gpu_engine_percent(&EngineSnapshot::new(), &b, 1.0), None);
        assert_eq!(gpu_engine_percent(&a, &b, 0.0), None);
    }
}
