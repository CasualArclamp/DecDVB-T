// made by CasualArclamp
//! Transport packets to radio bytes, in real time.
//!
//! `Modulator` (one OFDM symbol a call, 8448 samples for 8k 1/32) -> optional
//! interpolating resampler -> fixed gain and cs8 rounding in one pass. No
//! channel or radio simulation, no probe pass (that is `sim::generate`, for files). The level is
//! fixed from the modulator's known mean power, so the output never depends on
//! the content: `gain = 10^(level/20) / sqrt(mean_power())`.
//!
//! The first symbols are discarded (`Modulator::warmup_symbols`): until the
//! byte interleaver fills, every carrier carries the same point and the symbol
//! is a pulse 17 times the rms. They are fed null packets (the `priming` flag)
//! so no programme is lost to them. A few more are discarded so the signal
//! starts at the first symbol of a superframe (TPS frame 0, symbol 0).
//!
//! CPU, measured on this machine (one core, % per second of signal, `txfile
//! --stages`): 64-QAM 2/3 8k 1/32 modulator 7.2 (outer 0.8, inner 0.7, pilots,
//! IFFT and guard 5.7) and cs8 pack 0.4; 64-QAM 7/8 7.5 and 0.4; the 35/32
//! resampler to 10 MS/s 12 (24 to 20 MS/s). Before the speed work (inner and
//! outer chain by tables, ring-buffer resampler, one-pass gain and rounding)
//! these were 41.8 and 4.7, 92 and 8.5, and 18. The numbers are in
//! docs/TX_DESIGN.md §14.1.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use decdvbt_core::{Error, Iq, Result};
use decdvbt_dsp::Resampler;
use decdvbt_mod::Modulator;
use decdvbt_mod::ts::TsSource;

use crate::config::{TxConfig, TxRate};
use crate::preview::{PreviewQueue, PreviewTap};
use crate::ring::pack_cs8_scaled;

/// The occupied bandwidth of an 8 MHz DVB-T signal, Hz (the resampler keeps
/// this much and rejects the images beyond).
const OCCUPIED_8MHZ_HZ: f64 = 7.61e6;

/// The linear gain that makes `mean_power` (the modulator's, 1.08 for DVB-T)
/// come out at `level_dbfs` rms of |c|.
pub fn gain_for(level_dbfs: f32, mean_power: f64) -> f32 {
    (10f64.powf(level_dbfs as f64 / 20.0) / mean_power.sqrt()) as f32
}

/// What the generator measures, read by the session.
#[derive(Debug, Default)]
pub struct IqStats {
    /// Samples produced at the radio's rate, since the start.
    pub samples: AtomicU64,
    /// Nanoseconds spent producing them (modulating, resampling, packing),
    /// not counting time waiting for room in the ring: `busy_ns` over the
    /// signal's duration is the modulator load.
    pub busy_ns: AtomicU64,
    /// Samples with a component at full scale after rounding.
    pub clipped: AtomicU64,
    /// Output rms of |c| (dBFS) and peak component (0..1), as `f32` bits,
    /// renewed per block.
    pub rms_dbfs_bits: AtomicU32,
    pub peak_bits: AtomicU32,
}

impl IqStats {
    /// The level the last block measured: rms of |c| in dBFS, and the peak
    /// component (0..1, held with a slow decay).
    pub fn level(&self) -> (f32, f32) {
        (
            f32::from_bits(self.rms_dbfs_bits.load(Ordering::Relaxed)),
            f32::from_bits(self.peak_bits.load(Ordering::Relaxed)),
        )
    }
}

/// The real-time IQ source. Owns the modulator.
#[non_exhaustive]
pub struct IqGenerator {
    modulator: Modulator,
    gain: f32,
    resampler: Option<Resampler>,
    priming: Arc<AtomicBool>,
    stats: Arc<IqStats>,
    preview: Option<PreviewTap>,
    rate_hz: f64,
    /// The modulator's symbol, then the resampler's output.
    sym: Vec<Iq>,
    work: Vec<Iq>,
    /// Quantised samples read back from the bytes for the preview.
    tap_scratch: Vec<Iq>,
    /// Samples produced so far (the preview's clock).
    sample_index: u64,
    peak_hold: f32,
}

impl IqGenerator {
    /// Build the modulator on `ts`, the gain and (for a fixed rate) the
    /// resampler. `priming` is the flag `FeederTs` watches.
    pub fn new(
        cfg: &TxConfig,
        ts: Box<dyn TsSource>,
        priming: Arc<AtomicBool>,
        stats: Arc<IqStats>,
        preview: Option<PreviewQueue>,
    ) -> Result<IqGenerator> {
        cfg.validate()?;
        let modulator = Modulator::new(cfg.mode, None, ts).map_err(Error::Other)?;
        let gain = gain_for(cfg.level_dbfs, modulator.mean_power());
        let native = cfg.mode.bandwidth.sample_rate_hz();
        let (resampler, rate_hz) = match cfg.rate {
            TxRate::Native => (None, f64::from(cfg.sample_rate_hz())),
            TxRate::Fixed(hz) => {
                let occupied = OCCUPIED_8MHZ_HZ * cfg.mode.bandwidth.mhz() / 8.0;
                let r =
                    Resampler::for_rates(native, f64::from(hz), occupied).map_err(Error::Other)?;
                (Some(r), f64::from(hz))
            }
        };
        let rate_hz = rate_hz.max(1.0);
        Ok(IqGenerator {
            modulator,
            gain,
            resampler,
            priming,
            stats,
            preview: preview.map(|q| PreviewTap::new(q, rate_hz)),
            rate_hz,
            sym: Vec::new(),
            work: Vec::new(),
            tap_scratch: Vec::new(),
            sample_index: 0,
            peak_hold: 0.0,
        })
    }

    /// Discard the warm-up symbols with `priming` set, then clear it.
    pub fn warm_up(&mut self) {
        let start = Instant::now();
        let n = self.modulator.warmup_symbols();
        for _ in 0..n {
            self.modulator.next_symbol(&mut self.sym);
        }
        // On to the start of a superframe: TPS then begins at frame 0.
        while self.modulator.position() != (0, 0) {
            self.modulator.next_symbol(&mut self.sym);
        }
        self.priming.store(false, Ordering::Relaxed);
        tracing::debug!("modulator warmed up in {:?}", start.elapsed());
    }

    /// Append cs8 bytes to `out` until it holds at least `min_bytes`, whole
    /// symbols at a time. Never allocates once `out` has grown.
    pub fn fill(&mut self, out: &mut Vec<u8>, min_bytes: usize) {
        let t0 = Instant::now();
        let mut clipped = 0u64;
        let mut produced = 0u64;
        let mut measured = false;
        while out.len() < min_bytes {
            self.modulator.next_symbol(&mut self.sym);
            let block: &[Iq] = match &mut self.resampler {
                Some(r) => {
                    self.work.clear();
                    r.process(&self.sym, &mut self.work);
                    &self.work
                }
                None => &self.sym,
            };
            if !measured {
                // One symbol a block is enough to read the level from.
                measured = true;
                let (mut sum, mut peak) = (0.0f32, 0.0f32);
                for s in block {
                    sum += s.norm_sqr();
                    peak = peak.max(s.re.abs()).max(s.im.abs());
                }
                // The block is not yet gained (the gain goes in with the
                // rounding, in one pass), so the gain is applied here.
                let rms = (sum / block.len().max(1) as f32).sqrt() * self.gain;
                self.peak_hold = (peak * self.gain).max(self.peak_hold * 0.97);
                self.stats
                    .rms_dbfs_bits
                    .store((20.0 * rms.max(1e-9).log10()).to_bits(), Ordering::Relaxed);
                self.stats
                    .peak_bits
                    .store(self.peak_hold.to_bits(), Ordering::Relaxed);
            }
            let at = out.len();
            clipped += pack_cs8_scaled(block, self.gain * 128.0, out);
            if let Some(tap) = &mut self.preview
                && tap.is_due(self.sample_index, block.len())
            {
                // The row shows the bytes the radio gets, rounding and all.
                self.tap_scratch.clear();
                self.tap_scratch
                    .extend(out[at..].as_chunks::<2>().0.iter().map(|c| {
                        Iq::new(f32::from(c[0] as i8) / 128.0, f32::from(c[1] as i8) / 128.0)
                    }));
                tap.offer(&self.tap_scratch, self.sample_index);
            }
            self.sample_index += block.len() as u64;
            produced += block.len() as u64;
        }
        self.stats.samples.fetch_add(produced, Ordering::Relaxed);
        self.stats.clipped.fetch_add(clipped, Ordering::Relaxed);
        self.stats
            .busy_ns
            .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    /// The rate of the bytes, Hz.
    pub fn sample_rate_hz(&self) -> f64 {
        self.rate_hz
    }

    /// The linear gain applied to the modulator's output.
    pub fn gain(&self) -> f32 {
        self.gain
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_mod::ts::TestStream;

    #[test]
    fn gain_matches_the_measured_table() {
        // Scout's table: -12 dBFS -> 0.241, -20 -> 0.096 (mean power 1.08).
        assert!((gain_for(-12.0, 1.08) - 0.241).abs() < 0.001);
        assert!((gain_for(-20.0, 1.08) - 0.096).abs() < 0.001);
    }

    fn generator(cfg: &TxConfig) -> (IqGenerator, Arc<IqStats>) {
        let stats = Arc::new(IqStats::default());
        let g = IqGenerator::new(
            cfg,
            Box::<TestStream>::default(),
            Arc::new(AtomicBool::new(true)),
            stats.clone(),
            None,
        )
        .unwrap();
        (g, stats)
    }

    #[test]
    fn the_level_is_what_was_asked_and_nothing_clips() {
        // T9 in miniature (a 2k mode so a unit test stays quick): rms within
        // 0.2 dB of the request.
        let mut cfg = TxConfig::new("x.mp4".into());
        cfg.mode.fft = decdvbt_core::dvbt::FftSize::K2;
        let (mut g, stats) = generator(&cfg);
        g.warm_up();
        let mut bytes = Vec::new();
        g.fill(&mut bytes, 3_000_000);
        let sumsq: f64 = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| {
                let (i, q) = (f64::from(c[0] as i8) / 128.0, f64::from(c[1] as i8) / 128.0);
                i * i + q * q
            })
            .sum();
        let rms_db = 10.0 * (sumsq / (bytes.len() / 2) as f64).log10();
        assert!((rms_db + 12.0).abs() < 0.2, "{rms_db}");
        assert_eq!(stats.clipped.load(Ordering::Relaxed), 0);
        let (reported, peak) = stats.level();
        assert!((reported + 12.0).abs() < 0.5, "{reported}");
        assert!(peak > 0.0 && peak < 1.0);
        assert!(stats.busy_ns.load(Ordering::Relaxed) > 0);
        assert_eq!(
            stats.samples.load(Ordering::Relaxed) as usize,
            bytes.len() / 2
        );
    }

    #[test]
    fn warm_up_ends_on_a_superframe_boundary_and_clears_priming() {
        let mut cfg = TxConfig::new("x.mp4".into());
        cfg.mode.fft = decdvbt_core::dvbt::FftSize::K2;
        let (mut g, _) = generator(&cfg);
        assert!(g.priming.load(Ordering::Relaxed));
        g.warm_up();
        assert!(!g.priming.load(Ordering::Relaxed));
        assert_eq!(g.modulator.position(), (0, 0));
    }

    #[test]
    fn a_fixed_rate_resamples_and_reports_it() {
        let mut cfg = TxConfig::new("x.mp4".into());
        cfg.mode.fft = decdvbt_core::dvbt::FftSize::K2;
        cfg.rate = TxRate::Fixed(10_000_000);
        let (mut g, stats) = generator(&cfg);
        assert_eq!(g.sample_rate_hz(), 10e6);
        let mut bytes = Vec::new();
        g.fill(&mut bytes, 200_000);
        assert_eq!(
            stats.samples.load(Ordering::Relaxed) as usize,
            bytes.len() / 2
        );
        // 35/32 of the 2112-sample symbols.
        let per_symbol = 2112.0 * 35.0 / 32.0;
        let symbols = bytes.len() as f64 / 2.0 / per_symbol;
        assert!((symbols - symbols.round()).abs() < 0.02, "{symbols}");
    }

    #[test]
    fn the_preview_sees_the_bytes() {
        let mut cfg = TxConfig::new("x.mp4".into());
        cfg.mode.fft = decdvbt_core::dvbt::FftSize::K2;
        let q: PreviewQueue = Default::default();
        let mut g = IqGenerator::new(
            &cfg,
            Box::<TestStream>::default(),
            Arc::new(AtomicBool::new(true)),
            Arc::new(IqStats::default()),
            Some(q.clone()),
        )
        .unwrap();
        g.warm_up();
        let mut bytes = Vec::new();
        // 0.2 s of signal: about four rows.
        g.fill(&mut bytes, 2 * 1_830_000);
        let rows = q.lock().unwrap().len();
        assert!((3..=5).contains(&rows), "{rows}");
    }
}
