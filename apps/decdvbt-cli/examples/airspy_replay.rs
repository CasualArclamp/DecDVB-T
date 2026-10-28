// made by CasualArclamp
//! Replay a dump of raw Airspy transfers (u32 length, then the bytes, per
//! transfer) through the converter and the receiver, and say when the
//! decoder lost blocks. For comparing ways of handling the radio's glitches
//! on the same samples.
//!
//! `cargo run --release -p decdvbt-cli --example airspy_replay -- dump.raw packed|unpacked`

use decdvbt_core::dvbt::Bandwidth;
use decdvbt_io::airspy::convert::RealToIq;
use decdvbt_rx::auto::AutoReceiver;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let is_iq = decdvbt_io::format_from_path(args[0].as_ref()).is_some();
    let data = if is_iq {
        Vec::new()
    } else {
        std::fs::read(&args[0])?
    };
    let packed = args.get(1).is_none_or(|a| a == "packed");
    let fs = 10e6;
    let mut rx = AutoReceiver::with_bandwidth(fs, 0.0, Bandwidth::parse("8").unwrap());
    let mut conv = RealToIq::new();
    let (mut pos, mut abs, mut lost0, mut block, mut ts) = (0, 0u64, 0u64, Vec::new(), Vec::new());
    let mut losses = Vec::new();
    // OUT=path.cf32: the converted IQ as well.
    let mut out = match std::env::var("OUT") {
        Ok(p) => Some(decdvbt_io::IqFileWriter::create(
            p.as_ref(),
            decdvbt_core::SampleFormat::Cf32,
        )?),
        Err(_) => None,
    };
    // TRACE=from,to (seconds): a status line per transfer in that stretch.
    let trace: Option<(f64, f64)> = std::env::var("TRACE").ok().and_then(|v| {
        let (a, b) = v.split_once(',')?;
        Some((a.parse().ok()?, b.parse().ok()?))
    });
    // An IQ file instead of a dump: read as it is.
    let mut file = decdvbt_io::format_from_path(args[0].as_ref())
        .map(|f| decdvbt_io::IqFileReader::open(args[0].as_ref(), f, fs, 49152).unwrap());
    loop {
        if let Some(f) = file.as_mut() {
            use decdvbt_io::IqSource;
            if f.read(&mut block)? == 0 {
                break;
            }
        } else {
            if pos + 4 > data.len() {
                break;
            }
            let n = u32::from_le_bytes(data[pos..pos + 4].try_into()?) as usize;
            let bytes = &data[pos + 4..pos + 4 + n];
            pos += 4 + n;
            block.clear();
            if packed {
                conv.process_packed(bytes, &mut block);
            } else {
                conv.process_unpacked(bytes, &mut block);
            }
        }
        if let Some(w) = out.as_mut() {
            w.write(&block)?;
        }
        rx.push(&block, abs);
        abs += block.len() as u64;
        ts.clear();
        rx.take_ts(&mut ts);
        let st = rx.status();
        let t = abs as f64 / fs;
        if trace.is_some_and(|(a, b)| t >= a && t <= b) {
            println!(
                "{t:.4} sym {} snr {:.1} eff {:.1} cfo {:.1} ppm {:.3} coh {:.3} tps bad {} fixed {} lost {} dop {:?} mob {} den {}",
                st.symbols,
                st.snr_db,
                st.effective_snr_db,
                st.cfo_hz,
                st.clock_ppm,
                st.coherence,
                st.tps_frames_bad,
                st.outer.corrected,
                st.outer.uncorrectable,
                st.doppler_hz,
                st.mobile,
                st.denoise
            );
        }
        if st.outer.uncorrectable > lost0 {
            losses.push(format!(
                "{:.2}(+{})",
                abs as f64 / fs,
                st.outer.uncorrectable - lost0
            ));
            lost0 = st.outer.uncorrectable;
        }
    }
    let st = rx.status();
    println!(
        "{:.1} s, blanked {}, locks {}, RS fixed {} lost {}, SNR {:.1}, clock {:.2} ppm, cfo {:.1} Hz",
        abs as f64 / fs,
        conv.blanked(),
        st.locks,
        st.outer.corrected,
        st.outer.uncorrectable,
        st.snr_db,
        st.clock_ppm,
        st.cfo_hz
    );
    println!("losses: {}", losses.join(" "));
    Ok(())
}
