// made by CasualArclamp
//! A stand-in for ffmpeg, for the stress tests (`tests/stress.rs`): it
//! plays a transport stream file as if it were the encoder's output, and can
//! die or stall on cue.
//!
//! The session is given this program as `ffmpeg_path` and the test's `.ts`
//! file as the input (`-i FILE` among ffmpeg's arguments). Next to the file a
//! `FILE.mode` says what to do:
//!
//! - no file, or `all`: write the whole stream and exit 0;
//! - `die BYTES MS`: write that much, wait MS milliseconds, exit 3 (ffmpeg
//!   crashing);
//! - `stall BYTES MS`: write that much, wait MS milliseconds, write the rest,
//!   exit 0 (an encoder that stops for a while);
//! - `slow BYTES_PER_S`: write at that rate until the file ends.
//!
//! Called for anything but a stream (`-h muxer=...`, the NVENC test encode) it
//! exits 1 at once, which the transmitter reads as "not available".

use std::io::Write;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.last().map(String::as_str) != Some("pipe:1") {
        std::process::exit(1);
    }
    let Some(input) = args
        .iter()
        .position(|a| a == "-i")
        .and_then(|i| args.get(i + 1))
    else {
        std::process::exit(1);
    };
    let Ok(data) = std::fs::read(input) else {
        eprintln!("fake_ffmpeg: cannot read {input}");
        std::process::exit(1);
    };
    let mode = std::fs::read_to_string(format!("{input}.mode")).unwrap_or_default();
    let words: Vec<&str> = mode.split_whitespace().collect();
    let num = |i: usize| {
        words
            .get(i)
            .and_then(|w| w.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let mut out = std::io::stdout().lock();
    let write = |out: &mut std::io::StdoutLock, bytes: &[u8]| {
        // Whole packets only, like the real muxer.
        let n = bytes.len() / 188 * 188;
        if out.write_all(&bytes[..n]).is_err() || out.flush().is_err() {
            std::process::exit(0); // the reader went away
        }
    };
    match words.first().copied() {
        Some("die") => {
            let n = (num(1) as usize).min(data.len());
            write(&mut out, &data[..n]);
            std::thread::sleep(Duration::from_millis(num(2)));
            eprintln!("fake_ffmpeg: simulated crash");
            std::process::exit(3);
        }
        Some("stall") => {
            let n = (num(1) as usize).min(data.len());
            write(&mut out, &data[..n]);
            std::thread::sleep(Duration::from_millis(num(2)));
            write(&mut out, &data[n..]);
        }
        Some("slow") => {
            let rate = num(1).max(1) as f64;
            let t0 = Instant::now();
            let mut sent = 0usize;
            while sent < data.len() {
                let due = (t0.elapsed().as_secs_f64() * rate) as usize;
                let end = (due / 188 * 188).clamp(sent, data.len());
                if end > sent {
                    write(&mut out, &data[sent..end]);
                    sent = end;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        _ => write(&mut out, &data),
    }
}
