// made by CasualArclamp
//! A strong CW tone in the channel, taken out of the samples before anything
//! else sees them (frequency-shift-and-subtract).
//!
//! [`crate::notch`] erases the carriers a tone lands on after the transform. That
//! is enough for a weak tone, and costs nothing, but a strong one (a few dB
//! under the signal's power or more) does damage before the transform: the
//! cyclic-prefix correlation that finds the symbols, and the carrier-offset
//! estimate, both take a CW for a perfect echo and lock on to it, so the
//! receiver never acquires. The cure has to be in the time domain, and it
//! has to be cheap on a clean signal.
//!
//! **Detector** (always on, ~0.1 % of a core): every 524288 samples, an
//! 8192-point Hann-windowed FFT of the radio's samples, its power averaged over
//! the last few. A tone is a bin far over the local median (blocks of 128 bins,
//! interpolated) inside the channel's band: [`ON_DB`] to be taken up, [`OFF_DB`]
//! to be kept (a hysteresis). The OFDM signal's own continual pilots show as
//! lines too (8/3 in amplitude in T2's 8k and up: 10 dB over the data cells,
//! seen up to 17 dB over the median in a simulated 8k signal), hence the
//! thresholds; a tone of about −14 dB (of the signal's total power) shows 20 dB. The tone's frequency comes from a parabola
//! through the bins around the peak (~0.1 bin, 120 Hz).
//!
//! **Canceller** (only while a tone is held; ~1 ns a sample): an oscillator at
//! the tone's frequency, phase-continuous across blocks (and across gaps in the
//! stream), whose complex amplitude `a` is subtracted from every sample. Every
//! 2048 samples the sum of `x · conj(osc)` gives the tone's amplitude as it
//! seems from the oscillator; its turn since the last chunk is the frequency
//! error (a first-order loop on it follows a drifting or ppm-shifted tone), and
//! its smoothed value is the next chunk's `a`. The OFDM signal and noise add a
//! random −10·log₁₀(2048) dB to that sum, so the tone is taken out to the
//! level of this loop's noise: 20-30 dB for a tone of −10 dB, less for a
//! weaker one (and a weak one is left to the carrier erasure).
//!
//! Up to [`MAX_TONES`] tones are held. `DECDVBT_TONE=0` switches it off.

use std::sync::Arc;

use decdvbt_core::Iq;
use rustfft::{Fft, FftPlanner};

/// The detector's transform length, and samples between transforms.
const N: usize = 8192;
const STRIDE: usize = 524_288;
/// A tone's bin over the local median to be taken up and to be kept, dB.
pub const ON_DB: f32 = 20.0;
pub const OFF_DB: f32 = 14.0;
/// The average's rate (per transform), and transforms it takes before the
/// first look.
const RATE: f32 = 0.3;
const WARM_UP: u32 = 3;
/// Detector rounds a held tone may go unseen before it is let go.
const PATIENCE: u32 = 6;
/// The band looked at either side of the signal's centre, as a fraction of
/// the signal's own sample rate 1/T: 3.75 MHz at 8 MHz (the channel's 7.61 MHz
/// of carriers, less its filter skirts), 469 kHz for 1 MHz DVB-T, 757 kHz for
/// 1.7 MHz T2. (The detector's stride and transform are in the radio's
/// samples: at the 2 MS/s of a narrow signal it looks four times a second,
/// with 244 Hz bins.)
const HALF_BAND_OF_RATE: f64 = 3.75e6 / (64e6 / 7.0);
/// The local median's block and hop, bins.
const BLOCK: usize = 128;
const HOP: usize = 64;
/// Tones held at once.
pub const MAX_TONES: usize = 4;
/// Samples in a canceller chunk; the loop's gains.
const CHUNK: usize = 2048;
const BETA: f64 = 0.35;
const GAMMA: f32 = 0.5;

/// What the canceller is doing, for the status lines.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ToneStat {
    /// Tones held.
    pub tones: u32,
    /// The strongest's offset from the radio's centre, Hz, and its bin's
    /// power over the local median, dB.
    pub freq_hz: f32,
    pub over_db: f32,
}

struct Tone {
    /// Radians a sample, and the oscillator's phase at the next sample.
    w: f64,
    theta: f64,
    /// The amplitude being subtracted and the last chunk's measured one.
    a: Iq,
    prev: Iq,
    primed: bool,
    /// The chunk's running sum and length so far.
    sre: f32,
    sim: f32,
    count: usize,
    unseen: u32,
    over_db: f32,
}

impl Tone {
    fn new(w: f64, over_db: f32) -> Tone {
        Tone {
            w,
            theta: 0.0,
            a: Iq::new(0.0, 0.0),
            prev: Iq::new(0.0, 0.0),
            primed: false,
            sre: 0.0,
            sim: 0.0,
            count: 0,
            unseen: 0,
            over_db,
        }
    }

    /// Subtract the tone from `seg` (not past the chunk's end) and add to the
    /// chunk's sum. Eight independent oscillator lanes, each stepped by 8
    /// samples' rotation, so the loop has no serial dependence and
    /// vectorises.
    fn segment(&mut self, seg: &mut [Iq]) {
        let mut rr = [0f32; 8];
        let mut ri = [0f32; 8];
        for l in 0..8 {
            let (s, c) = (self.theta + self.w * l as f64).sin_cos();
            rr[l] = c as f32;
            ri[l] = s as f32;
        }
        let (s8, c8) = (self.w * 8.0).sin_cos();
        let (c8, s8) = (c8 as f32, s8 as f32);
        let (ar, ai) = (self.a.re, self.a.im);
        let mut sr = [0f32; 8];
        let mut si = [0f32; 8];
        let len = seg.len();
        let (body, rest) = seg.as_chunks_mut::<8>();
        for blk in body {
            for l in 0..8 {
                let x = blk[l];
                let (r, i) = (rr[l], ri[l]);
                sr[l] += x.re * r + x.im * i;
                si[l] += x.im * r - x.re * i;
                blk[l] = Iq::new(x.re - (ar * r - ai * i), x.im - (ar * i + ai * r));
                rr[l] = r * c8 - i * s8;
                ri[l] = r * s8 + i * c8;
            }
        }
        for (l, x) in rest.iter_mut().enumerate() {
            let (r, i) = (rr[l], ri[l]);
            sr[l] += x.re * r + x.im * i;
            si[l] += x.im * r - x.re * i;
            *x = Iq::new(x.re - (ar * r - ai * i), x.im - (ar * i + ai * r));
        }
        self.sre += sr.iter().sum::<f32>();
        self.sim += si.iter().sum::<f32>();
        self.count += len;
        self.theta = (self.theta + self.w * len as f64).rem_euclid(std::f64::consts::TAU);
        if self.count >= CHUNK {
            self.finish_chunk();
        }
    }

    fn finish_chunk(&mut self) {
        let am = Iq::new(self.sre / self.count as f32, self.sim / self.count as f32);
        (self.sre, self.sim, self.count) = (0.0, 0.0, 0);
        if !self.primed {
            (self.a, self.prev, self.primed) = (am, am, true);
            return;
        }
        // The turn of the measured amplitude since the last chunk is the
        // oscillator's frequency error times the chunk.
        let d = f64::from((am * self.prev.conj()).arg());
        self.prev = am;
        self.w += BETA * d / CHUNK as f64;
        let rot = Iq::from_polar(1.0, (d * (1.0 - BETA)) as f32);
        self.a = self.a * (1.0 - GAMMA) + am * rot * GAMMA;
        if !(self.a.re.is_finite() && self.a.im.is_finite()) {
            self.a = Iq::new(0.0, 0.0);
            self.primed = false;
        }
    }
}

pub struct ToneCanceller {
    enabled: bool,
    fs: f64,
    /// The signal's centre above the radio's, Hz.
    vfo: f64,
    /// The band either side of it that is searched, Hz.
    half_band: f64,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    buf: Vec<Iq>,
    scratch: Vec<Iq>,
    spec: Vec<f32>,
    level: Vec<f32>,
    meds: Vec<f32>,
    sel: Vec<f32>,
    rounds: u32,
    since: usize,
    tones: Vec<Tone>,
    out: Vec<Iq>,
    /// The next sample the stream should bring, to see a gap.
    next_abs: u64,
}

impl ToneCanceller {
    /// A canceller for the radio's samples at `fs`, the signal `vfo_offset_hz`
    /// above the radio's centre at its own rate `fs_native` (64/7 MHz for an
    /// 8 MHz channel), which sets the band searched.
    pub fn new(fs: f64, vfo_offset_hz: f64, fs_native: f64) -> ToneCanceller {
        let fft = FftPlanner::new().plan_fft_forward(N);
        let scratch = vec![Iq::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        let window = (0..N)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * (i as f32 + 0.5) / N as f32).cos())
            .collect();
        ToneCanceller {
            enabled: !std::env::var("DECDVBT_TONE").is_ok_and(|v| v == "0"),
            fs,
            vfo: vfo_offset_hz,
            half_band: HALF_BAND_OF_RATE * fs_native,
            fft,
            window,
            buf: vec![Iq::new(0.0, 0.0); N],
            scratch,
            spec: vec![0.0; N],
            level: vec![0.0; N],
            meds: Vec::new(),
            sel: Vec::new(),
            rounds: 0,
            since: STRIDE, // look at the first block
            tones: Vec::new(),
            out: Vec::new(),
            next_abs: 0,
        }
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        if !on {
            self.tones.clear();
        }
    }

    /// Debugging: the held tones' offsets from the radio's centre, Hz.
    pub fn held_hz(&self) -> Vec<f64> {
        self.tones
            .iter()
            .map(|t| t.w * self.fs / std::f64::consts::TAU)
            .collect()
    }

    pub fn stat(&self) -> ToneStat {
        let Some(t) = self
            .tones
            .iter()
            .max_by(|a, b| a.over_db.total_cmp(&b.over_db))
        else {
            return ToneStat::default();
        };
        ToneStat {
            tones: self.tones.len() as u32,
            freq_hz: (t.w * self.fs / std::f64::consts::TAU) as f32,
            over_db: t.over_db,
        }
    }

    /// Take the buffer [`Self::process`] cleaned into (to lend it to code that
    /// needs `self` as well), and give it back after.
    pub fn take_out(&mut self) -> Vec<Iq> {
        std::mem::take(&mut self.out)
    }

    pub fn give_back(&mut self, out: Vec<Iq>) {
        self.out = out;
    }

    /// Take the held tones out of the block, into [`Self::take_out`]'s
    /// buffer. False when there are none: use the input as it is (nothing was
    /// copied). `abs_start` is the block's first sample's place in the stream.
    pub fn process(&mut self, block: &[Iq], abs_start: u64) -> bool {
        if !self.enabled {
            return false;
        }
        self.look(block);
        let gap = abs_start.wrapping_sub(self.next_abs);
        self.next_abs = abs_start + block.len() as u64;
        if self.tones.is_empty() {
            return false;
        }
        self.out.clear();
        self.out.extend_from_slice(block);
        // A gap in the stream: the oscillators keep their frequency, the
        // chunk in progress is dropped.
        if abs_start != 0 && gap != 0 && gap < (1 << 40) {
            for t in &mut self.tones {
                t.theta = (t.theta + t.w * gap as f64).rem_euclid(std::f64::consts::TAU);
                (t.sre, t.sim, t.count) = (0.0, 0.0, 0);
            }
        }
        for t in &mut self.tones {
            let mut pos = 0;
            while pos < self.out.len() {
                let n = (CHUNK - t.count).min(self.out.len() - pos);
                t.segment(&mut self.out[pos..pos + n]);
                pos += n;
            }
        }
        true
    }

    /// Every so many samples, a transform of the start of the block.
    fn look(&mut self, block: &[Iq]) {
        self.since += block.len();
        if self.since < STRIDE || block.len() < N {
            return;
        }
        self.since = 0;
        for ((b, &x), &w) in self.buf.iter_mut().zip(block).zip(&self.window) {
            *b = x * w;
        }
        self.fft
            .process_with_scratch(&mut self.buf, &mut self.scratch);
        if self.rounds == 0 {
            for (s, b) in self.spec.iter_mut().zip(&self.buf) {
                *s = b.norm_sqr();
            }
        } else {
            for (s, b) in self.spec.iter_mut().zip(&self.buf) {
                *s += RATE * (b.norm_sqr() - *s);
            }
        }
        self.rounds += 1;
        if self.rounds < WARM_UP {
            return;
        }
        self.detect();
    }

    fn freq_of(&self, bin: usize) -> f64 {
        let k = if bin < N / 2 {
            bin as f64
        } else {
            bin as f64 - N as f64
        };
        k * self.fs / N as f64
    }

    fn in_band(&self, bin: usize) -> bool {
        (self.freq_of(bin) - self.vfo).abs() < self.half_band
    }

    /// The local median of the spectrum around each bin: blocks of
    /// [`BLOCK`] bins, a hop apart, linearly between their centres. The
    /// transform's bins run 0..N/2 then −N/2..0, so the median is taken over
    /// the shifted order to have no seam inside the band.
    fn levels(&mut self) {
        let blocks = (N - BLOCK).div_ceil(HOP) + 1;
        self.meds.clear();
        for b in 0..blocks {
            let a = (b * HOP).min(N - BLOCK);
            self.sel.clear();
            self.sel
                .extend((a..a + BLOCK).map(|i| self.spec[(i + N / 2) % N]));
            self.meds
                .push(*self.sel.select_nth_unstable_by(BLOCK / 2, f32::total_cmp).1);
        }
        for i in 0..N {
            let pos =
                ((i as f32 - BLOCK as f32 / 2.0) / HOP as f32).clamp(0.0, (blocks - 1) as f32);
            let j = pos as usize;
            let f = pos - j as f32;
            let m1 = self.meds[(j + 1).min(blocks - 1)];
            self.level[(i + N / 2) % N] = self.meds[j] + f * (m1 - self.meds[j]);
        }
    }

    fn detect(&mut self) {
        self.levels();
        let off = 10f32.powf(OFF_DB / 10.0);
        let on = 10f32.powf(ON_DB / 10.0);
        let mut seen = vec![false; self.tones.len()];
        for k in 0..N {
            if !self.in_band(k) {
                continue;
            }
            let (p, l) = (self.spec[k], self.level[k].max(1e-20));
            if p < off * l {
                continue;
            }
            // A local peak only (the window's main lobe is 4 bins wide).
            let near = |d: isize| self.spec[(k as isize + d).rem_euclid(N as isize) as usize];
            if (-2..=2).any(|d| d != 0 && near(d) > p) {
                continue;
            }
            // The parabola through the log powers of the bin and its two
            // neighbours.
            let (a, b, c) = (
                near(-1).max(1e-20).ln(),
                p.max(1e-20).ln(),
                near(1).max(1e-20).ln(),
            );
            let den = a - 2.0 * b + c;
            let frac = if den < -1e-6 {
                (0.5 * (a - c) / den).clamp(-0.5, 0.5)
            } else {
                0.0
            };
            let f = self.freq_of(k) + f64::from(frac) * self.fs / N as f64;
            let w = std::f64::consts::TAU * f / self.fs;
            let over_db = 10.0 * (p / l).log10();
            let bin_w = std::f64::consts::TAU / N as f64;
            if let Some((i, t)) = self
                .tones
                .iter_mut()
                .enumerate()
                .find(|(_, t)| (t.w - w).abs() < 2.5 * bin_w)
            {
                t.unseen = 0;
                t.over_db = over_db;
                seen[i] = true;
            } else if p >= on * l && self.tones.len() < MAX_TONES {
                self.tones.push(Tone::new(w, over_db));
                seen.push(true);
            }
        }
        let mut i = 0;
        self.tones.retain_mut(|t| {
            let keep = if seen[i] {
                true
            } else {
                t.unseen += 1;
                t.unseen < PATIENCE
            };
            i += 1;
            keep
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: &mut u64) -> Vec<Iq> {
        (0..n)
            .map(|_| {
                let mut g = || {
                    // Sum of uniforms: near enough Gaussian.
                    (0..6)
                        .map(|_| {
                            *seed = seed
                                .wrapping_mul(6364136223846793005)
                                .wrapping_add(1442695040888963407);
                            ((*seed >> 40) as f32 / (1u64 << 24) as f32) - 0.5
                        })
                        .sum::<f32>()
                };
                Iq::new(g(), g())
            })
            .collect()
    }

    /// Power of the component of `x` at `w` rad/sample (a least-squares fit).
    fn tone_power(x: &[Iq], w: f64) -> f32 {
        let mut s = Iq::new(0.0, 0.0);
        for (n, v) in x.iter().enumerate() {
            s += v * Iq::from_polar(1.0, -(w * n as f64) as f32);
        }
        (s / x.len() as f32).norm_sqr()
    }

    #[test]
    fn no_tone_passes_the_block_through_untouched() {
        let mut seed = 3;
        let mut c = ToneCanceller::new(10e6, 0.0, 64e6 / 7.0);
        for i in 0..8 {
            let b = noise(131_072, &mut seed);
            assert!(!c.process(&b, i * 131_072));
        }
        assert_eq!(c.stat(), ToneStat::default());
    }

    #[test]
    fn a_tone_is_found_and_taken_down_and_a_drifting_one_followed() {
        let fs = 10e6;
        let mut seed = 5;
        let mut c = ToneCanceller::new(fs, 0.0, 64e6 / 7.0);
        let block = 131_072usize;
        let (f0, drift) = (1.2345e6f64, 2000.0f64); // Hz, Hz/s
        let amp = 3.0f32; // noise power is 6/12 = 0.5: the tone is 18 dB over
        let mut phase = 0f64;
        let mut worst = 0f32;
        let mut stat = ToneStat::default();
        for i in 0..40u64 {
            let mut b = noise(block, &mut seed);
            for (n, x) in b.iter_mut().enumerate() {
                let t = (i as usize * block + n) as f64 / fs;
                phase += std::f64::consts::TAU * (f0 + drift * t) / fs;
                *x += Iq::from_polar(amp, phase as f32);
            }
            if c.process(&b, i * block as u64) {
                let out = c.take_out();
                if i >= 25 {
                    // What is left at the tone's own frequency this block.
                    let t = (i as usize * block) as f64 / fs;
                    let w = std::f64::consts::TAU * (f0 + drift * (t + 0.006)) / fs;
                    worst = worst.max(tone_power(&out, w));
                }
                c.give_back(out);
            } else if i >= 25 {
                panic!("block {i}: tone not held");
            }
            stat = c.stat();
        }
        assert_eq!(stat.tones, 1, "{stat:?}");
        assert!(stat.over_db > 17.0, "{stat:?}");
        assert!(
            (f64::from(stat.freq_hz) - (f0 + drift * 0.5)).abs() < 3000.0,
            "{stat:?}"
        );
        // From 9 (amp²) to under 0.01: at least 29 dB down.
        assert!(worst < 0.01, "left {worst}");
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// A benchmark: cargo test --release -p decdvbt-rx bench_tone -- --ignored --nocapture
    #[test]
    #[ignore = "a benchmark"]
    fn bench_tone() {
        let mut seed = 1u64;
        let mut u = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 40) as f32 / (1u64 << 24) as f32) - 0.5
        };
        let block: Vec<Iq> = (0..131_072).map(|_| Iq::new(u(), u())).collect();
        // Detector alone (no tone held): every block is looked at.
        let mut c = ToneCanceller::new(10e6, 0.0, 64e6 / 7.0);
        let mut best = f64::MAX;
        for r in 0..20 {
            c.since = STRIDE;
            let t = std::time::Instant::now();
            c.process(&block, r * 131_072);
            if r >= 5 {
                best = best.min(t.elapsed().as_secs_f64());
            }
        }
        println!(
            "detector pass (one 8192 FFT + medians): {:.1} us",
            best * 1e6
        );
        // The canceller on a block, one tone held.
        let mut t = Tone::new(0.7, 20.0);
        let mut seg = block.clone();
        let mut best = f64::MAX;
        for _ in 0..20 {
            let s = std::time::Instant::now();
            for ch in seg.chunks_mut(CHUNK) {
                t.segment(ch);
            }
            best = best.min(s.elapsed().as_secs_f64());
        }
        println!(
            "canceller: {:.2} ns a sample ({:.1} % of a core at 10 Msps)",
            best / block.len() as f64 * 1e9,
            best / block.len() as f64 * 1e7 * 100.0
        );
    }
}
