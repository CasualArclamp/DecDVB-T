// made by CasualArclamp
//! Passive radar from a DVB-T capture: each range–Doppler map as a PNG
//! (Doppler across, the whole ±541 Hz of 8K 1/32, grid every 100 Hz;
//! bistatic range up, grid every 25 km; colour 0–15 dB over the noise
//! floor), its figures and strongest echoes, and the CPU it took.
//!
//! `cargo run --release -p decdvbt-gui --example radar_probe -- CAPTURE [out-dir] [max-km] [seconds]`

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use decdvbt_core::{Iq, SampleFormat, bytes_to_iq};
use decdvbt_rx::radar::{RadarHistory, RangeDoppler, echoes};
use decdvbt_rx::receiver::{Receiver, ReceiverConfig};

fn colour(v: f32) -> [u8; 3] {
    const STOPS: [[f32; 3]; 5] = [
        [8.0, 8.0, 30.0],
        [70.0, 20.0, 110.0],
        [190.0, 50.0, 90.0],
        [250.0, 150.0, 40.0],
        [255.0, 250.0, 190.0],
    ];
    let x = v.clamp(0.0, 1.0) * (STOPS.len() - 1) as f32;
    let i = (x.floor() as usize).min(STOPS.len() - 2);
    let f = x - i as f32;
    std::array::from_fn(|c| (STOPS[i][c] + f * (STOPS[i + 1][c] - STOPS[i][c])) as u8)
}

fn save(m: &RangeDoppler, ranges: usize, path: &Path) {
    let (w, h) = (m.dopplers as u32, ranges as u32);
    let mut img = image::RgbImage::new(w, h);
    for r in 0..ranges {
        for d in 0..m.dopplers {
            let mut c = colour(m.at(r, d) / 15.0);
            let hz = m.doppler_hz(d);
            let km = m.range_km(r);
            if (hz - (hz / 100.0).round() * 100.0).abs() < m.doppler_step_hz / 2.0
                || (km - (km / 25.0).round() * 25.0).abs() < m.range_step_km / 2.0
            {
                c = c.map(|v| v.saturating_add(45));
            }
            img.put_pixel(d as u32, h - 1 - r as u32, image::Rgb(c));
        }
    }
    img.save(path).unwrap();
}

/// Mean dB over the cells in `km` and `hz` bands: the ridge, the comb.
fn band(m: &RangeDoppler, km: (f64, f64), f: &dyn Fn(f64) -> bool) -> f64 {
    let (mut s, mut c) = (0f64, 0usize);
    for r in 0..m.ranges {
        let k = m.range_km(r);
        if k >= km.0 && k < km.1 {
            for d in 0..m.dopplers {
                if f(m.doppler_hz(d)) {
                    s += m.at(r, d) as f64;
                    c += 1;
                }
            }
        }
    }
    s / c.max(1) as f64
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let path = Path::new(&a[1]);
    let out = PathBuf::from(a.get(2).map_or("out/radar", |s| s.as_str()));
    let max_km: f64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(100.0);
    let seconds: f64 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(1e9);
    std::fs::create_dir_all(&out).unwrap();
    let rate = 10e6;
    let mut f = std::fs::File::open(path).unwrap();
    let mut rx = Receiver::new(ReceiverConfig::new(rate, 0.0));
    rx.set_radar(true);
    let mut hist: Option<RadarHistory> = None;
    let (mut raw, mut iq, mut ts, mut rows) =
        (vec![0u8; 262_144], Vec::<Iq>::new(), Vec::new(), Vec::new());
    let (mut pos, limit) = (0u64, (seconds * rate) as u64);
    let (mut n, mut rx_s, mut map_s) = (0, 0.0, 0.0);
    let (mut ridge_sum, mut quarter_sum) = (0.0, 0.0);
    while pos < limit {
        let got = f.read(&mut raw).unwrap();
        if got < 2 {
            break;
        }
        bytes_to_iq(&raw[..got & !1], SampleFormat::Cs8, &mut iq);
        let t0 = Instant::now();
        rx.push(&iq, pos);
        rx_s += t0.elapsed().as_secs_f64();
        pos += iq.len() as u64;
        ts.clear();
        rx.take_ts(&mut ts);
        let Some((geo, gap)) = rx.take_radar(&mut rows) else {
            continue;
        };
        let h = hist
            .get_or_insert_with(|| RadarHistory::new(RadarHistory::cpi_for(1.0, geo.slot_s), geo));
        h.set_max_km(max_km);
        if h.geometry() != geo {
            *h = RadarHistory::new(RadarHistory::cpi_for(1.0, geo.slot_s), geo);
        } else if gap {
            h.clear();
        }
        let t1 = Instant::now();
        let due = h.push_rows(&mut rows);
        let m = if due { h.map() } else { None };
        map_s += t1.elapsed().as_secs_f64();
        let Some(m) = m else { continue };
        let ranges = ((max_km / m.range_step_km).ceil() as usize).min(m.ranges);
        save(&m, ranges, &out.join(format!("map_{n:03}.png")));
        // The clutter ridge (5–40 Hz against beyond 150 Hz, 5–60 km) and
        // the pilots' cycle (a quarter of the symbol rate, ±270 Hz, against
        // beside it, 0–2 km).
        let ridge = band(&m, (5.0, 60.0), &|h| (5.0..40.0).contains(&h.abs()))
            - band(&m, (5.0, 60.0), &|h| h.abs() > 150.0);
        let q = -m.doppler_start_hz / 2.0;
        let quarter = band(&m, (0.0, 2.0), &|h| (h.abs() - q).abs() < 1.0)
            - band(&m, (0.0, 2.0), &|h| ((h.abs() - q).abs() - 6.0).abs() < 1.5);
        ridge_sum += ridge;
        quarter_sum += quarter;
        println!(
            "map {n:3}  t {:6.1} s  floor {:6.1} dB under the direct path  max {:4.1} dB  ridge {ridge:4.1} dB  pilot cycle {quarter:4.1} dB",
            pos as f64 / rate,
            m.floor_db,
            m.max_snr_db
        );
        for e in echoes(&m, 15.0, max_km, 15.0, 6) {
            println!(
                "          {:6.2} km  {:+7.1} Hz  {:4.1} dB",
                e.range_km, e.doppler_hz, e.db
            );
        }
        n += 1;
    }
    let secs = pos as f64 / rate;
    let s = rx.status();
    println!(
        "mean over {n} maps: ridge {:.1} dB, pilot cycle {:.1} dB",
        ridge_sum / n.max(1) as f64,
        quarter_sum / n.max(1) as f64
    );
    println!(
        "{secs:.1} s: receiver {:.1} % of a core (radar profiles {:.1} %), maps {:.1} %; SNR {:.1} dB; notch {:?}",
        100.0 * rx_s / secs,
        100.0 * s.profile.radar.as_secs_f64() / secs,
        100.0 * map_s / secs,
        s.snr_db,
        s.notch
    );
}
