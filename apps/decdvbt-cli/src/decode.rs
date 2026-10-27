// made by CasualArclamp
//! `decdvbt decode`: run the receiver over a capture (or the live HackRF),
//! write the transport stream, and say what came out: the mode, the signal's
//! quality, the Reed–Solomon statistics and the services the stream holds.
//! A `.ts` file is read as it is (no receiver). `--carousel SID DIR` writes
//! the service's DSM-CC object carousel (its MHEG-5 application) to a folder.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use decdvbt_core::channel::{uhf_centre_hz, uhf_channel_at};
use decdvbt_rx::auto::{AutoReceiver, Standard};
use decdvbt_ts::TS_LEN;
use decdvbt_ts::dsmcc::ObjectCarousel;
use decdvbt_ts::psi::TsAnalyser;

use crate::source;

#[derive(clap::Args)]
pub struct DecodeArgs {
    /// An IQ file, a transport stream (`.ts`), or a UHF channel ("C26") for
    /// the HackRF or (with `--radio airspy`) the Airspy R2.
    source: String,
    /// Write the transport stream here.
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Seconds of signal to decode.
    #[arg(long, default_value_t = 6.0)]
    seconds: f64,
    /// The signal's offset from the radio's centre, Hz (a file's name gives
    /// the centre; the channel is taken as the one it falls in).
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    offset: f64,
    /// Sample rate of a raw file, MS/s, when its name does not say.
    #[arg(long, default_value_t = 10.0)]
    file_rate: f64,
    /// HackRF RF amplifier (+14 dB): needed for UHF television.
    #[arg(long)]
    amp: bool,
    /// HackRF LNA gain, dB (0-40, steps of 8).
    #[arg(long, default_value_t = 32)]
    lna: u16,
    /// HackRF VGA gain, dB (0-62, steps of 2).
    #[arg(long, default_value_t = 8)]
    vga: u16,
    #[command(flatten)]
    radio: source::RadioArgs,
    /// Print the guide: this many events of each service (of every
    /// multiplex the stream's guide covers), with their details.
    #[arg(long, default_value_t = 0)]
    guide: usize,
    /// The DVB-T2 PLP to decode, by id (the first data PLP if not given).
    #[arg(long)]
    plp: Option<u8>,
    /// Print a line a second while decoding.
    #[arg(long)]
    progress: bool,
    /// Leave packets the Reed–Solomon code could not correct out of the
    /// stream (they are kept, with the error flag set, by default).
    #[arg(long)]
    drop_errors: bool,
    /// Write service SID's object carousel (its MHEG-5 / Red Button
    /// application) to the folder DIR: `--carousel 4416 out/rb`.
    #[arg(long, num_args = 2, value_names = ["SID", "DIR"])]
    carousel: Vec<String>,
}

/// `--carousel`: the service's carousel, made once its PMT is in.
struct CarouselDump {
    sid: u16,
    dir: PathBuf,
    oc: Option<ObjectCarousel>,
    /// The PMT had no carousel.
    none: bool,
}

impl CarouselDump {
    fn new(args: &[String]) -> Result<Option<Self>> {
        let [sid, dir] = args else { return Ok(None) };
        Ok(Some(CarouselDump {
            sid: sid
                .parse()
                .context("--carousel: the service id is a number")?,
            dir: PathBuf::from(dir),
            oc: None,
            none: false,
        }))
    }

    fn packet(&mut self, an: &TsAnalyser, p: &[u8; TS_LEN]) {
        if let Some(oc) = self.oc.as_mut() {
            oc.packet(p);
        } else if !self.none
            && let Some(prog) = an.programmes.get(&self.sid)
            && !prog.streams.is_empty()
        {
            self.oc = ObjectCarousel::for_programme(prog);
            self.none = self.oc.is_none();
        }
    }

    fn finish(&self) -> Result<()> {
        let Some(oc) = &self.oc else {
            println!(
                "carousel: service {} {}",
                self.sid,
                if self.none {
                    "has no object carousel"
                } else {
                    "was not found"
                }
            );
            return Ok(());
        };
        let (done, modules) = oc.progress();
        let (tree, pending) = oc.tree();
        std::fs::create_dir_all(&self.dir)?;
        let files = oc.dump(&self.dir)?;
        println!(
            "carousel {} of service {} on PID(s) {:?}: {} of {} modules in ({} kB), {} files written to {}{}{}",
            oc.carousel_id,
            self.sid,
            oc.pids().collect::<Vec<_>>(),
            done,
            modules,
            oc.bytes() / 1000,
            files,
            self.dir.display(),
            if pending > 0 {
                format!(", {pending} objects still to come")
            } else {
                String::new()
            },
            if oc.has_gateway() {
                ""
            } else {
                " (no DSI yet)"
            }
        );
        for (path, kind, size) in tree.iter().take(60) {
            println!("  {kind} {size:>8} {path}");
        }
        if tree.len() > 60 {
            println!("  ... {} more", tree.len() - 60);
        }
        Ok(())
    }
}

/// A transport stream file: the analyser (and carousel) over it.
fn run_ts(a: &DecodeArgs, mut dump: Option<CarouselDump>) -> Result<()> {
    use std::io::Read;
    let t0 = Instant::now();
    let mut f = std::io::BufReader::new(
        std::fs::File::open(&a.source).with_context(|| format!("cannot open {}", a.source))?,
    );
    let mut analyser = TsAnalyser::new();
    let mut p = [0u8; TS_LEN];
    while f.read_exact(&mut p).is_ok() {
        if p[0] != decdvbt_ts::TS_SYNC {
            continue; // not aligned: the file is expected whole packets
        }
        analyser.packet(&p);
        if let Some(d) = dump.as_mut() {
            d.packet(&analyser, &p);
        }
    }
    println!("{} read in {:.2} s", a.source, t0.elapsed().as_secs_f64());
    summarise(&analyser);
    if a.guide > 0 {
        guide(&analyser, a.guide);
    }
    if let Some(d) = &dump {
        d.finish()?;
    }
    Ok(())
}

pub fn run(a: DecodeArgs) -> Result<()> {
    let mut dump = CarouselDump::new(&a.carousel)?;
    if a.source.to_ascii_lowercase().ends_with(".ts") {
        return run_ts(&a, dump);
    }
    let mut src = source::open(
        &a.source,
        a.file_rate,
        Some(source::Gains {
            amp: a.amp,
            lna: a.lna,
            vga: a.vga,
        }),
        &a.radio,
    )?;
    let fs = src.sample_rate();
    let centre = src.center_freq();
    let channel_centre = uhf_channel_at(centre).and_then(uhf_centre_hz);
    println!(
        "{} (centre {:.3} MHz{})",
        src.describe(),
        centre / 1e6,
        channel_centre.map_or(String::new(), |c| format!(", channel {:.3} MHz", c / 1e6))
    );
    let mut out = match &a.output {
        Some(p) => Some(std::io::BufWriter::new(
            std::fs::File::create(p).with_context(|| format!("cannot create {}", p.display()))?,
        )),
        None => None,
    };

    let mut rx = AutoReceiver::new(fs, a.offset + channel_centre.map_or(0.0, |c| c - centre));
    rx.set_plp(a.plp);
    rx.set_realtime(src.is_live());
    let mut analyser = TsAnalyser::new();
    let (mut abs, mut block, mut ts) = (0u64, Vec::new(), Vec::new());
    let (t0, mut in_rx, mut next_report) = (Instant::now(), Duration::ZERO, 1.0);
    let mut packets_out = 0u64;
    let mut lost_before = 0u64;
    while (abs as f64) < a.seconds * fs {
        if src.read(&mut block)? == 0 {
            break;
        }
        let t = Instant::now();
        rx.push(&block, abs);
        in_rx += t.elapsed();
        abs += block.len() as u64;
        ts.clear();
        rx.take_ts(&mut ts);
        for p in ts.as_chunks::<TS_LEN>().0 {
            if a.drop_errors && p[1] & 0x80 != 0 {
                continue;
            }
            analyser.packet(p);
            if let Some(d) = dump.as_mut() {
                d.packet(&analyser, p);
            }
            if let Some(w) = out.as_mut() {
                w.write_all(p)?;
            }
            packets_out += 1;
        }
        let secs = abs as f64 / fs;
        analyser.tick(block.len() as f64 / fs);
        if a.progress && secs >= next_report {
            next_report += 1.0;
            let st = rx.status();
            if rx.standard() == Standard::Dvbt2 {
                let t2 = rx.dvbt2();
                println!(
                    "{secs:5.1} s: DVB-T2, L1 {}, MER {:.1} dB, {} frames lost, LDPC blocks {} decoded / {} lost, {} packets",
                    if t2.l1_ok { "read" } else { "lost" },
                    t2.snr_db,
                    t2.frames_bad,
                    t2.blocks_ok,
                    t2.blocks_bad,
                    packets_out
                );
                if t2.frames_bad > lost_before {
                    lost_before = t2.frames_bad;
                    println!("      {}", t2.debug);
                }
                continue;
            }
            println!(
                "{secs:5.1} s: {} symbols, TPS {}, SNR {:.1} dB, RS {} clean / {} fixed / {} lost, {} packets; Doppler {}, estimate {}{}",
                st.symbols,
                if st.tps_synced {
                    "in sync"
                } else {
                    "searching"
                },
                st.snr_db,
                st.outer.clean,
                st.outer.corrected,
                st.outer.uncorrectable,
                packets_out,
                st.doppler_hz
                    .map_or("-".to_string(), |d| format!("{d:.0} Hz")),
                if st.mobile { "time filter" } else { "IIR" },
                if st.denoise { " + delay filter" } else { "" },
            );
        }
    }
    // The T2 receiver's FEC stage is a frame or two behind.
    rx.flush();
    ts.clear();
    rx.take_ts(&mut ts);
    for p in ts.as_chunks::<TS_LEN>().0 {
        if a.drop_errors && p[1] & 0x80 != 0 {
            continue;
        }
        analyser.packet(p);
        if let Some(d) = dump.as_mut() {
            d.packet(&analyser, p);
        }
        if let Some(w) = out.as_mut() {
            w.write_all(p)?;
        }
        packets_out += 1;
    }
    if let Some(mut w) = out {
        w.flush()?;
    }
    if let Some(d) = &dump {
        d.finish()?;
    }
    let signal_s = abs as f64 / fs;
    let took = t0.elapsed().as_secs_f64();
    println!(
        "{signal_s:.1} s of signal in {took:.2} s ({:.1} % of real time); \
         the receiver alone {:.0} ms ({:.1} % of real time)",
        100.0 * took / signal_s,
        in_rx.as_secs_f64() * 1e3,
        100.0 * in_rx.as_secs_f64() / signal_s
    );

    if rx.standard() == Standard::Dvbt2 {
        t2_report(&rx, signal_s, packets_out, a.output.as_deref());
        summarise(&analyser);
        if a.guide > 0 {
            guide(&analyser, a.guide);
        }
        return Ok(());
    }
    let st = rx.status();
    {
        let p = &st.profile;
        let pct = |d: Duration| 100.0 * d.as_secs_f64() / signal_s;
        println!(
            "where the time went (% of real time, one core): acquire {:.1}, read symbols {:.1},              equalise {:.1}, TPS {:.1}, demap {:.1}, Viterbi {:.1}, outer {:.1}",
            pct(p.acquire),
            pct(p.read),
            pct(p.equalise),
            pct(p.tps),
            pct(p.demap),
            pct(p.viterbi),
            pct(p.outer)
        );
    }
    if !st.tracking {
        let t2 = rx.dvbt2();
        println!(
            "no DVB-T lock: {} attempts, best cyclic-prefix contrast {:.1} (a lock needs 5);              no DVB-T2 lock either: {} attempts, {} without a P1 symbol, {} with one whose L1 could not be read",
            st.attempts, st.contrast, t2.attempts, t2.acq_no_p1, t2.acq_no_l1
        );
        return Ok(());
    }
    println!(
        "locked: {} guard {}, {} lock(s), carrier offset {:+.1} Hz from the radio's centre, \
         clock {:+.1} ppm",
        st.fft.map_or("?", |f| f.name()),
        st.guard.map_or("?", |g| g.name()),
        st.locks,
        st.cfo_hz,
        st.clock_ppm
    );
    match &st.tps {
        None => println!("TPS not found ({} frames read)", st.tps_frames_ok),
        Some(t) => {
            println!(
                "TPS: {} {} {}, hierarchy {:?}, {} frames read, {} bad",
                t.modulation.name(),
                t.rate_hp.name(),
                st.guard.map_or("?", |g| g.name()),
                t.alpha,
                st.tps_frames_ok,
                st.tps_frames_bad
            );
        }
    }
    if st.hierarchical {
        println!("hierarchical transmission is not decoded yet");
        return Ok(());
    }
    let o = &st.outer;
    println!(
        "SNR {:.1} dB mean, {:.1} dB effective; Reed-Solomon: {} blocks, {} clean, {} corrected ({} bytes), {} uncorrectable, \
         {} loss(es) of sync; error rate into the code {:.2e}",
        st.snr_db,
        st.effective_snr_db,
        o.blocks,
        o.clean,
        o.corrected,
        o.bytes_fixed,
        o.uncorrectable,
        o.losses,
        o.pre_rs_ber()
    );
    if let Some(l) = decdvbt_rx::notch::describe(&st.notch, &st.tone) {
        println!("narrowband interference: {l}");
    }
    println!(
        "{packets_out} transport packets{}",
        a.output
            .as_ref()
            .map_or(String::new(), |p| format!(" written to {}", p.display()))
    );
    summarise(&analyser);
    if a.guide > 0 {
        guide(&analyser, a.guide);
    }
    Ok(())
}

/// What the T2 receiver says after a run: the mode, the signal's quality, the
/// FEC blocks and where the time went.
fn t2_report(rx: &AutoReceiver, signal_s: f64, packets: u64, output: Option<&std::path::Path>) {
    let t2 = rx.dvbt2();
    let pct = |d: Duration| 100.0 * d.as_secs_f64() / signal_s;
    let p = &t2.profile;
    println!(
        "DVB-T2 lock: carrier {:+.1} Hz from the radio's centre, clock {:+.2} ppm, {} lock(s)",
        t2.cfo_hz, t2.clock_ppm, t2.locks
    );
    if let Some(m) = &t2.mode {
        println!(
            "mode: {} FFT{}, guard {}, {}",
            m.fft.name(),
            if m.extended { " extended" } else { "" },
            m.guard,
            m.pp
        );
    }
    if t2.plp_ids.len() > 1 {
        println!("PLPs in L1 (id, type): {:?}", t2.plp_ids);
    }
    if let Some(pl) = &t2.plp {
        println!(
            "PLP {}: {} {}{}, {}K LDPC, {} blocks a frame{}",
            t2.plp_id,
            pl.qam.name(),
            pl.rate,
            if pl.rotated { " rotated" } else { "" },
            if pl.frame == decdvbt_core::FecFrame::Normal {
                64
            } else {
                16
            },
            t2.blocks_per_frame,
            pl.qef_cn_db()
                .map_or(String::new(), |c| format!(" (needs about {c:.1} dB)")),
        );
    }
    println!(
        "{} frames read ({} lost); MER {:.1} dB; LDPC blocks {} decoded, {} lost, {:.1} iterations a block,          {} bit errors mended by BCH, {} resyncs",
        t2.frames_ok,
        t2.frames_bad,
        t2.snr_db,
        t2.blocks_ok,
        t2.blocks_bad,
        t2.ldpc_iterations as f64 / t2.blocks_ok.max(1) as f64,
        t2.bch_corrected,
        t2.resyncs
    );
    if let Some(l) = decdvbt_rx::notch::describe(&t2.notch, &t2.tone) {
        println!("narrowband interference: {l}");
    }
    println!(
        "where the time went (% of real time, one core): acquire {:.1}; read frames {:.1} (resample {:.1}, rotate {:.1},          transforms {:.1}); equalise {:.1} (alignment {:.1}, channel {:.1}, cells {:.1}); FEC stage {:.1}          of which deinterleave {:.1}, demap {:.1}, bit deinterleave {:.1}, LDPC {:.1}, BCH {:.1}",
        pct(p.acquire),
        pct(p.read),
        pct(p.resample),
        pct(p.rotate),
        pct(p.fft),
        pct(p.equalise),
        pct(p.align),
        pct(p.channel),
        pct(p.cells),
        pct(p.deinterleave + p.demap + p.gather + p.ldpc + p.bch),
        pct(p.deinterleave),
        pct(p.demap),
        pct(p.gather),
        pct(p.ldpc),
        pct(p.bch)
    );
    println!(
        "FEC stage: {} thread(s) of the most it may use, {:.0} % of real time on them{}",
        t2.fec_threads,
        100.0 * t2.fec_load,
        if t2.fec_dropped > 0 {
            format!(
                "; {} frame(s) given up, the stage being behind",
                t2.fec_dropped
            )
        } else {
            String::new()
        }
    );
    println!(
        "{packets} transport packets{}",
        output.map_or(String::new(), |p| format!(" written to {}", p.display()))
    );
}

/// The guide: for each service with events, by channel number, its next
/// `per` events with their marks, kinds, warnings and CRIDs.
fn guide(an: &TsAnalyser, per: usize) {
    let r = an.report();
    let epg = &an.epg;
    let now = r.utc_unix.unwrap_or(0);
    let mut rows: Vec<(Option<u16>, u16, String)> = r
        .programmes
        .iter()
        .map(|p| (p.lcn, p.number, p.name.clone().unwrap_or_default()))
        .chain(
            r.others
                .iter()
                .map(|o| (o.lcn, o.number, o.name.clone().unwrap_or_default())),
        )
        .filter(|(_, sid, _)| epg.services.get(sid).is_some_and(|m| !m.is_empty()))
        .collect();
    rows.sort_by_key(|(lcn, sid, _)| (lcn.unwrap_or(u16::MAX), *sid));
    println!(
        "guide: {} events, {} services ({} of other multiplexes)",
        epg.events(),
        rows.len(),
        rows.iter()
            .filter(|(_, sid, _)| r.others.iter().any(|o| o.number == *sid))
            .count()
    );
    for (lcn, sid, name) in rows {
        let events = epg.between(sid, now, i64::MAX);
        let last = events.last().map_or(0, |e| e.end());
        println!(
            "{:>4} {name} ({} events, to {})",
            lcn.map_or(String::new(), |n| n.to_string()),
            epg.services[&sid].len(),
            decdvbt_ts::epg::civil(last).2
        );
        for e in events.iter().take(per) {
            let (_, mo, d, h, mi, _) = decdvbt_ts::epg::civil(e.start);
            let mut marks = Vec::new();
            for (on, m) in [
                (e.hd, "HD"),
                (e.subtitles, "S"),
                (e.hard_of_hearing, "HoH"),
                (e.audio_description, "AD"),
                (e.signed, "SL"),
                (e.surround, "5.1"),
            ] {
                if on {
                    marks.push(m);
                }
            }
            let kinds = e.kinds();
            println!(
                "       {mo:02}-{d:02} {h:02}:{mi:02} UTC {:>3} min  {}  [{}] {}{}",
                e.duration_s / 60,
                e.name,
                marks.join(" "),
                kinds.join(", "),
                e.guidance
                    .as_ref()
                    .map_or(String::new(), |g| format!("  ! {g}"))
            );
            if let Some(c) = &e.series_crid {
                println!("              series {}", epg.full_crid(sid, c));
            }
            if let Some(c) = &e.episode_crid {
                println!("              episode {}", epg.full_crid(sid, c));
            }
        }
    }
}

/// What the PSI/SI of the stream says.
fn summarise(an: &TsAnalyser) {
    let r = an.report();
    let errors: u64 = r.pids.iter().map(|p| p.stats.errors).sum();
    let cc: u64 = r.pids.iter().map(|p| p.stats.cc_errors).sum();
    println!(
        "stream: {} packets, {} with the error flag, {} continuity jumps, {} bad sections{}",
        r.packets,
        errors,
        cc,
        r.bad_sections,
        r.utc
            .as_ref()
            .map_or(String::new(), |t| format!(", clock {t} UTC"))
    );
    if let Some(n) = &r.network.name {
        println!("network: {n}");
    }
    println!("{} service(s):", r.programmes.len());
    for p in &r.programmes {
        let streams: Vec<String> = p.streams.iter().map(|s| s.describe()).collect();
        println!(
            "  {:5} {:<24} {:<12} {}{}",
            p.number,
            p.name.as_deref().unwrap_or("?"),
            p.service_type.map_or(String::new(), |t| format!(
                "{t:#04x} {}",
                decdvbt_ts::service_type_name(t)
            )),
            streams.join(", "),
            if p.free_ca { "  [scrambled]" } else { "" }
        );
    }
}
