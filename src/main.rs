//! bc250mon — ultra-light monitor for AMD BC-250 nodes.
//! One static binary per node: samples CPU/GPU/MEM, keeps history in fixed rings
//! (optionally persisted), lists processes on demand and serves a single-page UI.
mod hist;
mod sys;

use hist::{Acc, Pt, Store, NONE16, NONE8};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const INDEX: &str = include_str!("index.html");
const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Cfg {
    listen: String,
    name: String,
    data: Option<PathBuf>,
    peers: Vec<(String, String)>,
    interval: Duration,
    days: u32,
    gpu_dev: Option<PathBuf>,
}

#[derive(Default, Clone)]
struct Now {
    ts: u64,
    cpu: f32,
    cores: Vec<u8>,
    gpu: Option<f32>,
    mem: sys::Mem,
    vram: Option<(u64, u64)>,
    gtt: Option<(u64, u64)>,
    gtemp: Option<f32>,
    ctemp: Option<f32>,
    pwr: Option<f32>,
    sclk: Option<u32>,
    load: [f32; 3],
    uptime: u64,
}

struct State {
    now: Now,
    fine: hist::Ring,
    coarse: Store,
}

fn usage() -> ! {
    eprintln!(
        "bc250mon {VERSION}
usage: bc250mon [options]
  --listen ADDR      bind address (default 0.0.0.0:9250; use the VPN IP, e.g. 10.8.0.11:9250)
  --name NAME        node display name (default: hostname)
  --peers LIST       nodes shown in the UI: name=http://ip:port,name2=http://ip2:port
                     (default: only this node)
  --data FILE        history file (default /var/lib/bc250mon/history.bin, 'none' = RAM only)
  --days N           days of 1-minute history to keep (default 7; 20 bytes/minute)
  --interval SEC     sample interval in seconds (default 2)
  --gpu-dev PATH     amdgpu sysfs device dir (default: auto-detect)"
    );
    std::process::exit(2)
}

fn parse_args() -> Cfg {
    let mut c = Cfg {
        listen: "0.0.0.0:9250".into(),
        name: sys::hostname(),
        data: Some("/var/lib/bc250mon/history.bin".into()),
        peers: vec![],
        interval: Duration::from_secs(2),
        days: 7,
        gpu_dev: None,
    };
    let mut a = std::env::args().skip(1);
    while let Some(k) = a.next() {
        let mut v = || a.next().unwrap_or_else(|| usage());
        match k.as_str() {
            "--listen" => c.listen = v(),
            "--name" => c.name = v(),
            "--data" => { let d = v(); c.data = if d == "none" { None } else { Some(d.into()) } }
            "--days" => c.days = v().parse().unwrap_or_else(|_| usage()),
            "--interval" => c.interval = Duration::from_secs_f32(v().parse::<f32>().unwrap_or_else(|_| usage()).max(0.5)),
            "--gpu-dev" => c.gpu_dev = Some(v().into()),
            "--peers" => {
                for item in v().split(',').filter(|s| !s.is_empty()) {
                    let (n, u) = item.split_once('=').unwrap_or((item, item));
                    c.peers.push((n.trim().into(), u.trim().trim_end_matches('/').into()));
                }
            }
            "-V" | "--version" => { println!("bc250mon {VERSION}"); std::process::exit(0) }
            _ => usage(),
        }
    }
    c
}

fn unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn permille(used: u64, total: u64) -> u16 {
    if total == 0 { NONE16 } else { ((used as f64 * 1000.0 / total as f64).round() as u16).min(1000) }
}

fn sampler(st: Arc<Mutex<State>>, interval: Duration, gpu: Option<sys::Gpu>) {
    const SUB: u32 = 4; // GPU busy is instantaneous; sub-sample it
    let ctemp_path = sys::cpu_temp_path();
    let mut prev = sys::cpu_times();
    let mut acc = Acc::default();
    loop {
        let (mut gsum, mut gmax, mut gn) = (0u64, 0u64, 0u64);
        for _ in 0..SUB {
            std::thread::sleep(interval / SUB);
            if let Some(b) = gpu.as_ref().and_then(|g| g.busy()) {
                gsum += b;
                gmax = gmax.max(b);
                gn += 1;
            }
        }
        let cur = sys::cpu_times();
        let cpu = if !cur.is_empty() && !prev.is_empty() { sys::pct(prev[0], cur[0]) } else { 0.0 };
        let cores: Vec<u8> = cur.iter().skip(1).zip(prev.iter().skip(1)).map(|(b, a)| sys::pct(*a, *b).round() as u8).collect();
        prev = cur;
        let g = gpu.as_ref();
        let now = Now {
            ts: unix(),
            cpu,
            cores,
            gpu: if gn > 0 { Some(gsum as f32 / gn as f32) } else { None },
            mem: sys::meminfo(),
            vram: g.and_then(|g| g.mem("vram")),
            gtt: g.and_then(|g| g.mem("gtt")),
            gtemp: g.and_then(|g| g.temp()),
            ctemp: ctemp_path.as_ref().and_then(|p| sys::read_u64(p)).map(|v| v as f32 / 1000.0),
            pwr: g.and_then(|g| g.power()),
            sclk: g.and_then(|g| g.sclk()),
            load: sys::loadavg(),
            uptime: sys::uptime(),
        };
        let o8 = |v: Option<f32>| v.map(|x| x.round().clamp(0.0, 254.0) as u8).unwrap_or(NONE8);
        let pt = Pt {
            ts: now.ts as u32,
            cpu: o8(Some(cpu)),
            cpu_max: o8(Some(cpu)),
            gpu: o8(now.gpu),
            gpu_max: if gn > 0 { gmax.min(254) as u8 } else { NONE8 },
            mem: permille(now.mem.total - now.mem.avail.min(now.mem.total), now.mem.total),
            vram: now.vram.map(|(t, u)| permille(u, t)).unwrap_or(NONE16),
            gtt: now.gtt.map(|(t, u)| permille(u, t)).unwrap_or(NONE16),
            gtemp: o8(now.gtemp),
            ctemp: o8(now.ctemp),
            pwr: now.pwr.map(|w| (w * 10.0).round().clamp(0.0, 65534.0) as u16).unwrap_or(NONE16),
            swap: if now.mem.swap_total == 0 { 0 } else { permille(now.mem.swap_total - now.mem.swap_free.min(now.mem.swap_total), now.mem.swap_total) },
        };
        let done = acc.add(&pt);
        let mut s = st.lock().unwrap();
        s.now = now;
        s.fine.push(pt);
        if let Some(m) = done {
            s.coarse.push(m);
        }
    }
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "null".into())
}

fn now_json(cfg: &Cfg, n: &Now) -> String {
    let pair = |p: Option<(u64, u64)>| p.map(|(t, u)| format!("{{\"total\":{t},\"used\":{u}}}")).unwrap_or_else(|| "null".into());
    let mut name = String::new();
    sys::esc(&cfg.name, &mut name);
    let mut disks = String::new();
    for (i, (m, dev, fs, t, u, a)) in sys::disks().iter().enumerate() {
        if i > 0 { disks.push(','); }
        disks.push_str("{\"mnt\":");
        sys::esc(m, &mut disks);
        disks.push_str(",\"dev\":");
        sys::esc(dev, &mut disks);
        disks.push_str(&format!(",\"fs\":\"{fs}\",\"total\":{t},\"used\":{u},\"avail\":{a}}}"));
    }
    format!(
        "{{\"name\":{name},\"ts\":{},\"uptime\":{},\"load\":[{:.2},{:.2},{:.2}],\"cpu\":{:.1},\"cores\":{:?},\"gpu\":{},\
\"mem\":{{\"total\":{},\"avail\":{},\"swapTotal\":{},\"swapFree\":{}}},\"vram\":{},\"gtt\":{},\
\"gtemp\":{},\"ctemp\":{},\"pwr\":{},\"sclk\":{},\"selfRss\":{},\"disks\":[{}]}}",
        n.ts, n.uptime, n.load[0], n.load[1], n.load[2], n.cpu, n.cores,
        opt(n.gpu.map(|g| format!("{g:.1}"))),
        n.mem.total * 1024, n.mem.avail * 1024, n.mem.swap_total * 1024, n.mem.swap_free * 1024,
        pair(n.vram), pair(n.gtt),
        opt(n.gtemp.map(|v| format!("{v:.0}"))), opt(n.ctemp.map(|v| format!("{v:.0}"))),
        opt(n.pwr.map(|v| format!("{v:.1}"))), opt(n.sclk), sys::self_rss_kb() * 1024, disks
    )
}

fn info_json(cfg: &Cfg) -> String {
    let mut s = String::from("{\"name\":");
    sys::esc(&cfg.name, &mut s);
    s.push_str(&format!(",\"version\":\"{VERSION}\",\"interval\":{},\"days\":{},\"peers\":[", cfg.interval.as_secs_f32(), cfg.days));
    for (i, (n, u)) in cfg.peers.iter().enumerate() {
        if i > 0 { s.push(','); }
        s.push_str("{\"name\":");
        sys::esc(n, &mut s);
        s.push_str(",\"url\":");
        sys::esc(u, &mut s);
        s.push('}');
    }
    s.push_str("]}");
    s
}

fn qp<'a>(q: &'a str, key: &str) -> Option<&'a str> {
    q.split('&').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
}

fn respond(s: &mut TcpStream, code: &str, ctype: &str, body: &[u8]) {
    let h = format!(
        "HTTP/1.1 {code}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\n\
Access-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(h.as_bytes());
    let _ = s.write_all(body);
}

fn handle(mut s: TcpStream, cfg: &Cfg, st: &Mutex<State>) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 2048];
    let mut n = 0;
    while n < buf.len() {
        match s.read(&mut buf[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => {
                n += k;
                if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") { break; }
            }
        }
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let mut parts = req.lines().next().unwrap_or("").split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    if method == "OPTIONS" {
        return respond(&mut s, "204 No Content", "text/plain", b"");
    }
    if method != "GET" && method != "HEAD" {
        return respond(&mut s, "405 Method Not Allowed", "text/plain", b"GET only\n");
    }
    let (mut path, q) = target.split_once('?').unwrap_or((target, ""));
    const JSON: &str = "application/json";
    // /peer/<i>/api/... : served locally if peer i is this node, otherwise relayed to the
    // configured peer URL (lets the browser reach nodes it has no route to, e.g. node2
    // behind node1). Only configured peers and /api/ paths: not an open proxy.
    if let Some(rest) = path.strip_prefix("/peer/") {
        let (idx, sub) = rest.split_once('/').unwrap_or((rest, ""));
        let sub_path = &path[path.len() - sub.len() - 1..];
        let Some((pname, purl)) = idx.parse::<usize>().ok().and_then(|i| cfg.peers.get(i)) else {
            return respond(&mut s, "404 Not Found", "text/plain", b"no such peer\n");
        };
        if !sub_path.starts_with("/api/") {
            return respond(&mut s, "404 Not Found", "text/plain", b"not found\n");
        }
        if *pname == cfg.name {
            path = sub_path;
        } else {
            let tgt = if q.is_empty() { sub_path.to_string() } else { format!("{sub_path}?{q}") };
            return match http_get(purl, &tgt) {
                Some(body) => respond(&mut s, "200 OK", JSON, &body),
                None => respond(&mut s, "502 Bad Gateway", "text/plain", b"peer unreachable\n"),
            };
        }
    }
    match path {
        "/" | "/index.html" => respond(&mut s, "200 OK", "text/html; charset=utf-8", INDEX.as_bytes()),
        "/api/info" => respond(&mut s, "200 OK", JSON, info_json(cfg).as_bytes()),
        "/api/now" => {
            let n = st.lock().unwrap().now.clone();
            respond(&mut s, "200 OK", JSON, now_json(cfg, &n).as_bytes())
        }
        "/api/hist" => {
            let sec: u32 = qp(q, "sec").and_then(|v| v.parse().ok()).unwrap_or(3600).clamp(60, 400 * 86400);
            let maxn: u32 = qp(q, "n").and_then(|v| v.parse().ok()).unwrap_or(600).clamp(10, 3000);
            let to = unix() as u32;
            let from = to.saturating_sub(sec);
            let body = {
                let g = st.lock().unwrap();
                if sec <= 3600 {
                    // 2 s ring; anything older than its first sample comes from the 1-min ring
                    let first = g.fine.iter().next().map(|p| p.ts).unwrap_or(u32::MAX);
                    let older = g.coarse.ring.iter().filter(|p| p.ts + 60 <= first);
                    hist::query(older.chain(g.fine.iter()), from, to, cfg.interval.as_secs().max(1) as u32, maxn)
                } else {
                    hist::query(g.coarse.ring.iter(), from, to, 60, maxn)
                }
            };
            respond(&mut s, "200 OK", JSON, body.as_bytes())
        }
        "/api/procs" => {
            let lim: usize = qp(q, "n").and_then(|v| v.parse().ok()).unwrap_or(300).clamp(10, 5000);
            let body = sys::procs_json(lim, Duration::from_millis(500));
            respond(&mut s, "200 OK", JSON, body.as_bytes())
        }
        _ => respond(&mut s, "404 Not Found", "text/plain", b"not found\n"),
    }
}

/// Minimal HTTP/1.0 GET to http://host:port, returns body of a 200 response.
fn http_get(base: &str, path: &str) -> Option<Vec<u8>> {
    use std::net::ToSocketAddrs;
    let hostport = base.strip_prefix("http://")?.split('/').next()?;
    let addr = if hostport.contains(':') { hostport.to_string() } else { format!("{hostport}:80") };
    let sa = addr.to_socket_addrs().ok()?.next()?;
    let mut c = TcpStream::connect_timeout(&sa, Duration::from_secs(2)).ok()?;
    let _ = c.set_read_timeout(Some(Duration::from_secs(8)));
    let _ = c.set_write_timeout(Some(Duration::from_secs(3)));
    c.write_all(format!("GET {path} HTTP/1.0\r\nHost: {hostport}\r\nConnection: close\r\n\r\n").as_bytes()).ok()?;
    let mut buf = Vec::with_capacity(16 * 1024);
    c.take(8 << 20).read_to_end(&mut buf).ok()?;
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    if !buf.starts_with(b"HTTP/1.1 200") && !buf.starts_with(b"HTTP/1.0 200") {
        return None;
    }
    Some(buf.split_off(end + 4))
}

fn main() {
    let cfg = Arc::new(parse_args());
    let gpu = sys::Gpu::detect(cfg.gpu_dev.clone());
    if gpu.is_none() {
        eprintln!("bc250mon: no amdgpu device with gpu_busy_percent found; GPU metrics disabled");
    }
    let fine_cap = (3600.0 / cfg.interval.as_secs_f32()).ceil() as usize + 2;
    let coarse = Store::open(cfg.data.as_deref(), (cfg.days.max(1) * 1440) as usize);
    let st = Arc::new(Mutex::new(State { now: Now::default(), fine: hist::Ring::new(fine_cap), coarse }));
    {
        let st = st.clone();
        let iv = cfg.interval;
        std::thread::Builder::new().name("sampler".into()).stack_size(128 * 1024)
            .spawn(move || sampler(st, iv, gpu)).expect("spawn sampler");
    }
    let l = TcpListener::bind(&cfg.listen).unwrap_or_else(|e| {
        eprintln!("bc250mon: bind {}: {e}", cfg.listen);
        std::process::exit(1)
    });
    eprintln!("bc250mon {VERSION}: '{}' listening on http://{}", cfg.name, cfg.listen);
    static ACTIVE: AtomicUsize = AtomicUsize::new(0);
    for c in l.incoming().flatten() {
        if ACTIVE.load(Ordering::Relaxed) >= 8 {
            continue; // drop: too many concurrent requests
        }
        ACTIVE.fetch_add(1, Ordering::Relaxed);
        let (cfg, st) = (cfg.clone(), st.clone());
        let r = std::thread::Builder::new().stack_size(128 * 1024).spawn(move || {
            handle(c, &cfg, &st);
            ACTIVE.fetch_sub(1, Ordering::Relaxed);
        });
        if r.is_err() {
            ACTIVE.fetch_sub(1, Ordering::Relaxed);
        }
    }
}
