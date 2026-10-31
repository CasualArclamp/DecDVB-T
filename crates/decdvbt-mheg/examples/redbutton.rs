// made by CasualArclamp
//! Run a service's MHEG-5 application without the GUI and save what it
//! draws:
//!
//! ```text
//! cargo run --release -p decdvbt-mheg --example redbutton -- out/c23.ts --sid 4416 \
//!     --keys 100,wait,2,15 --png out/rb.png [--trace] [--seconds 5]
//! cargo run --release -p decdvbt-mheg --example redbutton -- out/rb4416   # a dumped carousel
//! ```
//!
//! A `.ts` file is read whole into the service's carousel first; a folder is
//! taken as a carousel already dumped (`decdvbt decode --carousel`). Keys are
//! UK codes (100 red, 1–4 arrows, 15 select, 16 back, 5–14 digits) or
//! `wait` (a second); after the keys the engine runs `--seconds` more.

use std::path::{Path, PathBuf};

use decdvbt_mheg::ic::{Http, IcFiles};
use decdvbt_mheg::{Engine, Fetch, Files, Renderer};

/// What the interaction channel's requests say they come from.
const USER_AGENT: &str = decdvbt_mheg::ic::USER_AGENT;
use decdvbt_ts::{ObjectCarousel, TS_LEN, TsAnalyser};

struct Dir(PathBuf);

impl Files for Dir {
    fn fetch(&self, path: &str) -> Fetch {
        match std::fs::read(self.0.join(path.trim_start_matches('/'))) {
            Ok(b) => Fetch::Ready(b),
            Err(_) => Fetch::Missing,
        }
    }
}

fn carousel_from_ts(path: &Path, sid: u16) -> (ObjectCarousel, decdvbt_mheg::Service) {
    let b = std::fs::read(path).expect("read the stream");
    let mut an = TsAnalyser::new();
    let mut oc: Option<ObjectCarousel> = None;
    for p in b.as_chunks::<TS_LEN>().0 {
        an.packet(p);
        match oc.as_mut() {
            Some(c) => c.packet(p),
            None => {
                if let Some(prog) = an.programmes.get(&sid)
                    && !prog.streams.is_empty()
                {
                    oc = ObjectCarousel::for_programme(prog);
                }
            }
        }
    }
    let oc = oc.expect("the service has no carousel");
    let (done, all) = oc.progress();
    eprintln!("carousel: {done} of {all} modules");
    let (onid, tsid) = (an.original_network_id.unwrap_or(0), an.ts_id.unwrap_or(0));
    let service = decdvbt_mheg::Service {
        original_network_id: onid,
        transport_stream_id: tsid,
        service_id: sid,
        services: an
            .programmes
            .keys()
            .map(|&s| (onid, tsid, s))
            .chain(an.others.values().map(|o| (onid, o.ts_id, o.number)))
            .collect(),
    };
    (oc, service)
}

fn save_png(r: &Renderer, path: &Path) {
    use decdvbt_mheg::render::{HEIGHT, WIDTH};
    // Over a dark grey, as if over a picture.
    let mut out = Vec::with_capacity(WIDTH * HEIGHT * 3);
    for p in r.rgba.chunks(4) {
        let a = p[3] as u32;
        for &c in &p[..3] {
            out.push(((c as u32 * a + 40 * (255 - a)) / 255) as u8);
        }
    }
    let f = std::fs::File::create(path).expect("create the png");
    let mut e = png::Encoder::new(std::io::BufWriter::new(f), WIDTH as u32, HEIGHT as u32);
    e.set_color(png::ColorType::Rgb);
    e.set_depth(png::BitDepth::Eight);
    e.write_header().unwrap().write_image_data(&out).unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let src = PathBuf::from(args.first().expect("a .ts file or a folder"));
    let opt = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let sid: u16 = opt("--sid").map_or(4416, |s| s.parse().unwrap());
    let seconds: f64 = opt("--seconds").map_or(3.0, |s| s.parse().unwrap());
    let keys = opt("--keys").unwrap_or_default();
    let png = opt("--png").map(PathBuf::from);
    let trace = args.iter().any(|a| a == "--trace");

    let oc;
    let dir;
    let mut service = decdvbt_mheg::Service {
        service_id: sid,
        ..Default::default()
    };
    let files: &dyn Files = if src.is_dir() {
        dir = Dir(src.clone());
        &dir
    } else {
        let (c, s) = carousel_from_ts(&src, sid);
        oc = c;
        service = s;
        &oc
    };
    // --ic: the interaction channel, files over HTTP(S) from the servers in
    // the carousel's auth.servers; time then runs at the real rate.
    let ic = args.iter().any(|a| a == "--ic");
    let http = ic.then(|| Http::new(USER_AGENT));
    let icf;
    let files: &dyn Files = match &http {
        Some(h) => {
            icf = IcFiles {
                carousel: files,
                http: h,
            };
            &icf
        }
        None => files,
    };
    let mut e = Engine::new();
    // --nv FILE: the receiver's non-volatile store ("pst://"), read before
    // and written after (Freeview's internet channels keep a device id).
    let nv = opt("--nv").map(PathBuf::from);
    if let Some(b) = nv.as_ref().and_then(|p| std::fs::read(p).ok()) {
        e.load_nonvolatile(&b);
    }
    e.ic = ic;
    e.service = service;
    e.tracing = trace;
    e.trace_stderr = args.iter().any(|a| a == "--live-trace");
    let mut r = Renderer::new();
    eprintln!("font: {}", r.font_name);
    let t0 = std::time::Instant::now();
    e.boot(files);
    let mut now = 0u64;
    let mut step = |e: &mut Engine, ms: u64| {
        let end = now + ms;
        while now < end {
            now += 40;
            e.tick(now, files);
            if ic {
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
        }
    };
    step(&mut e, 1000);
    for k in keys.split(',').filter(|k| !k.is_empty()) {
        if k == "wait" {
            step(&mut e, 1000);
        } else {
            e.key(k.parse().expect("a key code"));
            step(&mut e, 200);
        }
    }
    step(&mut e, (seconds * 1000.0) as u64);
    let busy = t0.elapsed();
    let t1 = std::time::Instant::now();
    r.render(&e);
    eprintln!(
        "app {:?}, scene {:?}, {} objects shown; engine {:.1} ms, drawing {:.1} ms{}",
        e.app(),
        e.scene(),
        r.rgba.chunks(4).filter(|p| p[3] != 0).count(),
        busy.as_secs_f64() * 1e3,
        t1.elapsed().as_secs_f64() * 1e3,
        e.error
            .as_ref()
            .map_or(String::new(), |x| format!(", error: {x}"))
    );
    for r in &e.requests {
        eprintln!("  request: {r:?}");
    }
    if let Some(h) = &http {
        for l in h.log() {
            eprintln!("  http: {} {}", l.outcome, l.url);
            // Show what a server worked out (its replies are small).
            if l.url.contains("?op=")
                && let Fetch::Ready(b) = h.fetch(&l.url)
            {
                eprintln!(
                    "    -> {:?}",
                    String::from_utf8_lossy(&b[..b.len().min(2000)])
                );
            }
        }
    }
    for s in e.shown() {
        eprintln!("  shown: {s}");
    }
    if trace {
        for l in &e.trace {
            println!("{l}");
        }
    }
    if let Some(p) = &nv {
        std::fs::write(p, e.save_nonvolatile()).expect("write the non-volatile store");
    }
    if let Some(p) = png {
        save_png(&r, &p);
        eprintln!("wrote {}", p.display());
    }
}
