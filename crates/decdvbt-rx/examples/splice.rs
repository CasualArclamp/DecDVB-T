// made by CasualArclamp
//! What a retune does to a receiver, offline and on real signals: feed the
//! first capture's samples up to some point, then the second capture from its
//! start, as the radio does when the samples queued from the old channel are
//! taken for the new one's (both captures are centred on their own channel, so
//! to the receiver the old signal and the new are at the same place). Counts
//! the transport packets that come out of the second capture's first seconds,
//! for splice points spread over the first.
//!
//! `cargo run --release -p decdvbt-rx --example splice -- A.cs8 B.cs8 [points] [seconds of B]`

use decdvbt_core::Iq;
use decdvbt_rx::receiver::{Receiver, ReceiverConfig};

fn load(path: &str, seconds: f64) -> Vec<Iq> {
    let bytes = std::fs::read(path).expect("a .cs8 file");
    let n = ((seconds * 10e6) as usize * 2).min(bytes.len());
    bytes[..n]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| Iq::new(f32::from(b[0] as i8) / 128.0, f32::from(b[1] as i8) / 128.0))
        .collect()
}

/// Packets out of B's first `b_len` samples after A's first `cut`.
fn run(a: &[Iq], b: &[Iq], cut: usize) -> (usize, u32) {
    let mut rx = Receiver::new(ReceiverConfig::new(10e6, 0.0));
    let mut ts = Vec::new();
    let mut at = 0u64;
    let feed = |rx: &mut Receiver, x: &[Iq], at: &mut u64| {
        for block in x.chunks(100_003) {
            rx.push(block, *at);
            *at += block.len() as u64;
        }
    };
    feed(&mut rx, &a[..cut], &mut at);
    rx.take_ts(&mut ts);
    ts.clear();
    feed(&mut rx, b, &mut at);
    rx.take_ts(&mut ts);
    (ts.len() / 188, rx.status().locks)
}

/// The receiver's state every quarter second after one splice.
fn trace(a: &[Iq], b: &[Iq], cut: usize) {
    let mut rx = Receiver::new(ReceiverConfig::new(10e6, 0.0));
    let mut at = 0u64;
    for block in a[..cut].chunks(100_003) {
        rx.push(block, at);
        at += block.len() as u64;
    }
    let mut ts = Vec::new();
    for (i, block) in b.chunks(2_500_000).enumerate() {
        for sub in block.chunks(100_003) {
            rx.push(sub, at);
            at += sub.len() as u64;
        }
        rx.take_ts(&mut ts);
        let st = rx.status();
        println!(
            "+{:.2} s: tracking {} tps {} decoding {} coherence {:.2} MER {:.1} cfo {:+.0} Hz clock {:+.1} ppm              locks {} stalled {} symbols {} RS good {} lost {} ts {} attempts {} contrast {:.1}",
            (i + 1) as f64 * 0.25,
            st.tracking,
            st.tps_synced,
            st.decoding,
            st.coherence,
            st.snr_db,
            st.cfo_hz,
            st.clock_ppm,
            st.locks,
            st.stalled_locks,
            st.symbols,
            st.outer.clean + st.outer.corrected,
            st.outer.uncorrectable,
            ts.len() / 188,
            st.attempts,
            st.contrast
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let points: usize = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(20);
    let b_seconds: f64 = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(1.5);
    let a = load(&args[1], 3.0);
    let b = load(&args[2], b_seconds);
    if let Some(cut) = std::env::var("TRACE")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
    {
        trace(&a, &b, (cut * 10e6) as usize);
        return;
    }
    if std::env::var_os("ACQSCAN").is_some() {
        // First-attempt success against where the stretch falls on the symbols.
        let mut fails = 0;
        for off in (0..8960).step_by(250) {
            let mut rx = Receiver::new(ReceiverConfig::new(10e6, 0.0));
            let mut at = off as u64;
            for block in b[off..off + 1_500_000].chunks(100_003) {
                rx.push(block, at);
                at += block.len() as u64;
            }
            let locked = rx.status().locks > 0;
            fails += usize::from(!locked);
            println!(
                "start offset {off:5}: {}",
                if locked { "locked" } else { "NO LOCK" }
            );
        }
        println!("{fails} first attempts of 36 failed");
        return;
    }
    let (clean, _) = run(&a, &b, 0);
    println!("B alone: {clean} packets in {b_seconds} s");
    let mut bad = 0;
    for k in 0..points {
        // Splice points from 0.4 s on (the receiver is locked by then), at
        // steps that are not a whole number of symbols.
        let cut = (0.4e7 + k as f64 * 0.0371e7) as usize + 1234 * k;
        let (n, locks) = run(&a, &b, cut.min(a.len()));
        let ok = n * 10 > clean * 6;
        bad += usize::from(!ok);
        println!(
            "splice at {:.3} s: {n} packets of B{}  ({locks} locks)",
            cut as f64 / 10e6,
            if ok { "" } else { "   <-- LOST" }
        );
    }
    println!("{bad} of {points} splices did not recover");
}
