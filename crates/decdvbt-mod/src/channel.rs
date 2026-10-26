// made by CasualArclamp
//! Channel models: what the air, the aerial and the radio do to the signal
//! on its way to the receiver, for testing it against known faults.
//!
//! In the order they are applied:
//!
//! 1. **Multipath**: any number of paths, each a delay (fractional, by a
//!    windowed-sinc interpolator, so a 0 dB echo makes the nulls it should),
//!    a gain and phase, a Doppler shift, and optionally Rayleigh or Rician
//!    fading (Clarke's sum of sinusoids at the path's maximum Doppler).
//! 2. (the receiver's filter and sample rate: the resampler)
//! 3. **The radio**, [`RadioSim`], at its own rate: white Gaussian noise at a
//!    C/N set against the nominal channel bandwidth, optional impulse bursts,
//!    a carrier frequency offset, IQ gain and phase imbalance, a DC offset
//!    (the HackRF's centre spike) and a sampling clock offset.
//!
//! Everything is seeded: the same [`Channel`] and signal give the same output.
//! The standard's own profiles F1 and P1 (EN 300 744 Annex B, 20 paths each)
//! are not here; the COST 207 ones are, and any set of paths can be typed.

use std::f64::consts::TAU;

use decdvbt_core::Iq;
use decdvbt_dsp::resample::bessel_i0;

use crate::rng::Rng;

/// How a path's gain varies in time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fading {
    Static,
    /// Rayleigh: no direct component, Doppler spread to ±`max_doppler_hz`.
    Rayleigh {
        max_doppler_hz: f64,
    },
    /// Rician: a steady component `k_db` over the scattered one.
    Rician {
        k_db: f64,
        max_doppler_hz: f64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Path {
    pub delay_us: f64,
    /// Power relative to the transmitted signal.
    pub gain_db: f64,
    pub phase_deg: f64,
    /// A steady Doppler shift of this path.
    pub doppler_hz: f64,
    pub fading: Fading,
}

impl Path {
    pub fn echo(delay_us: f64, gain_db: f64) -> Path {
        Path {
            delay_us,
            gain_db,
            phase_deg: 0.0,
            doppler_hz: 0.0,
            fading: Fading::Static,
        }
    }

    /// "DELAY_US:GAIN_DB[:PHASE_DEG[:DOPPLER_HZ]]", e.g. "2.5:-3".
    pub fn parse(s: &str) -> Result<Path, String> {
        let f: Vec<f64> = s
            .split(':')
            .map(|p| {
                p.trim()
                    .parse::<f64>()
                    .map_err(|_| format!("not a number: {p:?} in {s:?}"))
            })
            .collect::<Result<_, _>>()?;
        if !(2..=4).contains(&f.len()) {
            return Err(format!(
                "a path is DELAY_US:GAIN_DB[:PHASE_DEG[:DOPPLER_HZ]], not {s:?}"
            ));
        }
        if f[0] < 0.0 {
            return Err("a path's delay cannot be negative (put the earliest at 0)".into());
        }
        Ok(Path {
            delay_us: f[0],
            gain_db: f[1],
            phase_deg: f.get(2).copied().unwrap_or(0.0),
            doppler_hz: f.get(3).copied().unwrap_or(0.0),
            fading: Fading::Static,
        })
    }
}

/// Named sets of paths. Gains are powers (dB) relative to the transmitted
/// signal; the profile's total is whatever they sum to.
pub fn profile(name: &str) -> Option<Vec<Path>> {
    let set = |v: &[(f64, f64)]| v.iter().map(|&(d, g)| Path::echo(d, g)).collect();
    Some(match name.to_ascii_lowercase().as_str() {
        // COST 207 typical urban, six taps.
        "tu6" => set(&[
            (0.0, -3.0),
            (0.2, 0.0),
            (0.5, -2.0),
            (1.6, -6.0),
            (2.3, -8.0),
            (5.0, -10.0),
        ]),
        // COST 207 rural area, six taps.
        "ra6" => set(&[
            (0.0, 0.0),
            (0.1, -4.0),
            (0.2, -8.0),
            (0.3, -12.0),
            (0.4, -16.0),
            (0.5, -20.0),
        ]),
        // A single-frequency network's far transmitter: an echo inside a
        // 1/32 guard (28 µs).
        "sfn20" => set(&[(0.0, 0.0), (20.0, -3.0)]),
        // Two equal paths: the nulls a 0 dB echo makes, every 1/delay.
        "0db-echo" => set(&[(0.0, 0.0), (1.0, 0.0)]),
        // The look of the captures made from the test aerial: a strong
        // echo a couple of µs behind and a weaker one further on. A guess
        // fitted by eye, not a measurement.
        "urban" => set(&[(0.0, 0.0), (1.9, -3.0), (5.2, -8.0)]),
        _ => return None,
    })
}

pub const PROFILES: &[&str] = &["tu6", "ra6", "sfn20", "0db-echo", "urban"];

/// Bursts of impulsive noise.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Impulse {
    /// Bursts a second, on average (a Poisson process).
    pub rate_hz: f64,
    /// Power in a burst relative to the signal's, dB.
    pub rel_db: f64,
    pub width_us: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    /// Empty: one undelayed path at 0 dB.
    pub paths: Vec<Path>,
    pub cfo_hz: f64,
    /// Positive: the receiver's sampling clock runs this much fast.
    pub sco_ppm: f64,
    /// C/N against the nominal channel bandwidth, dB; `None` is noiseless.
    pub cn_db: Option<f64>,
    pub impulse: Option<Impulse>,
    /// DC offset as a fraction of the signal's rms amplitude, on I and Q.
    pub dc_offset: f64,
    /// IQ gain mismatch (dB) and phase error (degrees).
    pub iq_gain_db: f64,
    pub iq_phase_deg: f64,
    pub seed: u64,
}

impl Default for Channel {
    fn default() -> Self {
        Channel {
            paths: Vec::new(),
            cfo_hz: 0.0,
            sco_ppm: 0.0,
            cn_db: None,
            impulse: None,
            dc_offset: 0.0,
            iq_gain_db: 0.0,
            iq_phase_deg: 0.0,
            seed: 1,
        }
    }
}

impl Channel {
    /// Give every path Rayleigh fading at `max_doppler_hz`.
    pub fn with_rayleigh(mut self, max_doppler_hz: f64) -> Channel {
        for p in &mut self.paths {
            p.fading = Fading::Rayleigh { max_doppler_hz };
        }
        self
    }

    /// Give every path Rician fading: a steady component `k_db` over the
    /// scattered one, which spreads to ±`max_doppler_hz`.
    pub fn with_rician(mut self, k_db: f64, max_doppler_hz: f64) -> Channel {
        for p in &mut self.paths {
            p.fading = Fading::Rician {
                k_db,
                max_doppler_hz,
            };
        }
        self
    }

    pub fn is_clean(&self) -> bool {
        self.paths.is_empty()
            && self.cfo_hz == 0.0
            && self.sco_ppm == 0.0
            && self.cn_db.is_none()
            && self.impulse.is_none()
            && self.dc_offset == 0.0
            && self.iq_gain_db == 0.0
            && self.iq_phase_deg == 0.0
    }
}

/// Taps each side of the fractional-delay interpolator, and the delay every
/// path gets on top of its own so that the interpolator never needs a sample
/// it has not been given.
const HALF: usize = 12;
const BULK: usize = HALF;
const KAISER_BETA: f64 = 8.0;

/// The time-varying gain's evaluation step, samples: the fading is sampled
/// this often and interpolated between.
const FADE_STEP: usize = 32;
/// Oscillators in a Clarke sum.
const OSCILLATORS: usize = 24;

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
    }
}

/// A Kaiser-windowed sinc at `t` (zero beyond ±`half`).
fn kaiser_sinc(t: f64, half: f64) -> f64 {
    let r = t / half;
    if r.abs() >= 1.0 {
        return 0.0;
    }
    sinc(t) * bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / bessel_i0(KAISER_BETA)
}

/// Taps for a delay of `f` samples (0 ≤ f < 1), for x[n − d − m], m = −H+1 … H.
fn fractional_taps(f: f64) -> Vec<f32> {
    let h: Vec<f64> = (0..2 * HALF)
        .map(|j| {
            let m = j as f64 - (HALF as f64 - 1.0);
            kaiser_sinc(m - f, HALF as f64)
        })
        .collect();
    let sum: f64 = h.iter().sum();
    h.iter().map(|&c| (c / sum) as f32).collect()
}

/// Clarke's model: a sum of plane waves arriving at random angles.
struct Fade {
    /// Per oscillator: radians a sample and starting phase.
    osc: Vec<(f64, f64)>,
    los: f32,
    scatter: f32,
    g0: Iq,
    g1: Iq,
    count: usize,
    /// Samples at `g1`.
    t: f64,
}

impl Fade {
    fn new(max_doppler_hz: f64, k_db: Option<f64>, fs: f64, rng: &mut Rng) -> Fade {
        let osc = (0..OSCILLATORS)
            .map(|_| {
                let alpha = TAU * rng.uniform();
                (TAU * max_doppler_hz * alpha.cos() / fs, TAU * rng.uniform())
            })
            .collect();
        let (los, scatter) = match k_db {
            Some(k) => {
                let k = 10f64.powf(k / 10.0);
                (
                    (k / (k + 1.0)).sqrt() as f32,
                    (1.0 / (k + 1.0)).sqrt() as f32,
                )
            }
            None => (0.0, 1.0),
        };
        let mut f = Fade {
            osc,
            los,
            scatter,
            g0: Iq::new(0.0, 0.0),
            g1: Iq::new(0.0, 0.0),
            count: 0,
            t: 0.0,
        };
        f.g1 = f.eval(0.0);
        f.g0 = f.g1;
        f
    }

    fn eval(&self, t: f64) -> Iq {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for &(w, p) in &self.osc {
            let (s, c) = (w * t + p).sin_cos();
            re += c;
            im += s;
        }
        let n = (self.osc.len() as f64).sqrt();
        Iq::new(
            self.los + self.scatter * (re / n) as f32,
            self.scatter * (im / n) as f32,
        )
    }

    fn next(&mut self) -> Iq {
        if self.count == FADE_STEP {
            self.t += FADE_STEP as f64;
            self.g0 = self.g1;
            self.g1 = self.eval(self.t);
            self.count = 0;
        }
        let a = self.count as f32 / FADE_STEP as f32;
        self.count += 1;
        self.g0 + (self.g1 - self.g0) * a
    }
}

/// A path's complex gain, a sample at a time.
struct GainGen {
    base: Iq,
    /// Doppler: the phasor and its step.
    doppler: Option<(Iq, Iq)>,
    fade: Option<Fade>,
    n: u32,
}

impl GainGen {
    fn next(&mut self) -> Iq {
        let mut g = self.base;
        if let Some((ph, step)) = &mut self.doppler {
            g *= *ph;
            *ph *= *step;
            self.n += 1;
            // The phasor's length drifts with rounding: pull it back now and
            // then.
            if self.n.is_multiple_of(4096) {
                *ph /= ph.norm();
            }
        }
        if let Some(f) = &mut self.fade {
            g *= f.next();
        }
        g
    }

    fn is_constant(&self) -> bool {
        self.doppler.is_none() && self.fade.is_none()
    }
}

struct PathState {
    /// Whole samples of delay, including `BULK`.
    d_int: usize,
    /// The interpolator, or `None` for a whole-sample delay.
    taps: Option<Vec<f32>>,
    gain: GainGen,
}

struct ImpulseState {
    p_start: f64,
    width: usize,
    remaining: usize,
    sigma: f64,
}

/// A sampling clock offset by interpolation: the input read at positions
/// `r` apart instead of 1.
struct Sco {
    r: f64,
    /// Next output's position, in input samples, absolute.
    pos: f64,
    /// Absolute index of `tail[0]`.
    base: i64,
    tail: Vec<Iq>,
    table: Vec<[f32; 16]>,
}

const SCO_PHASES: usize = 512;

impl Sco {
    fn new(ppm: f64) -> Sco {
        let table = (0..SCO_PHASES)
            .map(|p| {
                let f = p as f64 / SCO_PHASES as f64;
                let mut t = [0f64; 16];
                for (q, c) in t.iter_mut().enumerate() {
                    *c = kaiser_sinc(f + 7.0 - q as f64, 8.0);
                }
                let sum: f64 = t.iter().sum();
                std::array::from_fn(|q| (t[q] / sum) as f32)
            })
            .collect();
        Sco {
            r: 1.0 / (1.0 + ppm * 1e-6),
            pos: 7.0,
            base: 0,
            tail: Vec::new(),
            table,
        }
    }

    fn process(&mut self, input: &[Iq], out: &mut Vec<Iq>) {
        self.tail.extend_from_slice(input);
        let end = self.base + self.tail.len() as i64;
        loop {
            let i = self.pos.floor() as i64;
            if i + 8 >= end {
                break;
            }
            let f = self.pos - i as f64;
            let ph = ((f * SCO_PHASES as f64).round() as usize).min(SCO_PHASES - 1);
            let first = (i - 7 - self.base) as usize;
            let mut acc = Iq::new(0.0, 0.0);
            for (q, &c) in self.table[ph].iter().enumerate() {
                acc += self.tail[first + q] * c;
            }
            out.push(acc);
            self.pos += self.r;
        }
        let keep_from =
            ((self.pos.floor() as i64 - 7 - self.base).max(0) as usize).min(self.tail.len());
        self.tail.drain(..keep_from);
        self.base += keep_from as i64;
    }
}

/// Propagation, at the signal's own sample rate: the paths, with their
/// delays, gains and fading. Feed it blocks, get blocks back, whatever their
/// size. The receiver's own faults are [`RadioSim`]'s, after the resampler.
pub struct ChannelSim {
    paths: Vec<PathState>,
    hist: Vec<Iq>,
    hist_len: usize,
    ext: Vec<Iq>,
    carrier_power: f64,
}

impl ChannelSim {
    /// Set up the paths of `ch` for a signal at `fs_hz` with mean power
    /// `signal_power` per sample.
    pub fn new(ch: &Channel, fs_hz: f64, signal_power: f64) -> ChannelSim {
        let mut rng = Rng::new(ch.seed);
        let direct = [Path::echo(0.0, 0.0)];
        let specs: &[Path] = if ch.paths.is_empty() {
            &direct
        } else {
            &ch.paths
        };
        let mut paths = Vec::new();
        let mut total_gain = 0.0;
        let mut max_d = 0;
        for p in specs {
            let amp = 10f64.powf(p.gain_db / 20.0);
            total_gain += amp * amp;
            let phase = p.phase_deg.to_radians();
            let base = Iq::new((amp * phase.cos()) as f32, (amp * phase.sin()) as f32);
            let delay = p.delay_us * 1e-6 * fs_hz + BULK as f64;
            let d_int = delay.floor() as usize;
            let frac = delay - d_int as f64;
            max_d = max_d.max(d_int);
            let fade = match p.fading {
                Fading::Static => None,
                Fading::Rayleigh { max_doppler_hz } => {
                    Some(Fade::new(max_doppler_hz, None, fs_hz, &mut rng))
                }
                Fading::Rician {
                    k_db,
                    max_doppler_hz,
                } => Some(Fade::new(max_doppler_hz, Some(k_db), fs_hz, &mut rng)),
            };
            let doppler = (p.doppler_hz != 0.0).then(|| {
                let w = TAU * p.doppler_hz / fs_hz;
                (Iq::new(1.0, 0.0), Iq::new(w.cos() as f32, w.sin() as f32))
            });
            paths.push(PathState {
                d_int,
                taps: (frac > 1e-9).then(|| fractional_taps(frac)),
                gain: GainGen {
                    base,
                    doppler,
                    fade,
                    n: 0,
                },
            });
        }
        let hist_len = max_d + HALF + 1;
        ChannelSim {
            paths,
            hist: vec![Iq::new(0.0, 0.0); hist_len],
            hist_len,
            ext: Vec::new(),
            carrier_power: signal_power * total_gain,
        }
    }

    /// The mean power of what comes out: the signal's, times the paths'
    /// summed power (a fading path averages its set gain).
    pub fn carrier_power(&self) -> f64 {
        self.carrier_power
    }

    /// Samples every path is late by, at least: the output lags the input by
    /// this much.
    pub fn bulk_delay() -> usize {
        BULK
    }

    /// Run `x` through the paths, appending to `out`.
    pub fn process(&mut self, x: &[Iq], out: &mut Vec<Iq>) {
        self.ext.clear();
        self.ext.extend_from_slice(&self.hist);
        self.ext.extend_from_slice(x);
        out.reserve(x.len());
        for n in 0..x.len() {
            let i = self.hist_len + n;
            let mut y = Iq::new(0.0, 0.0);
            for p in &mut self.paths {
                let v = match &p.taps {
                    None => self.ext[i - p.d_int],
                    Some(t) => {
                        // y[n] = Σ_m x[n − d − m]·h[m], m = −H+1 … H: the
                        // newest sample read is x[n − d + H − 1].
                        let newest = i - p.d_int + HALF - 1;
                        let mut acc = Iq::new(0.0, 0.0);
                        for (j, &c) in t.iter().enumerate() {
                            acc += self.ext[newest - j] * c;
                        }
                        acc
                    }
                };
                y += if p.gain.is_constant() {
                    v * p.gain.base
                } else {
                    v * p.gain.next()
                };
            }
            out.push(y);
        }
        // Keep the newest samples for the next block's filters.
        let keep = self.ext.len() - self.hist_len;
        self.hist.copy_from_slice(&self.ext[keep..]);
    }
}

/// The receiver, at its own sample rate, after its filter: noise, impulse
/// bursts, a carrier offset, IQ imbalance, a DC offset and a sampling clock
/// offset. (Noise added before the resampler would be shaped by its roll-off;
/// a real radio's noise is flat to the edge of what it captures.)
pub struct RadioSim {
    rng: Rng,
    /// Noise per component, and the powers it was set from.
    sigma: f64,
    carrier_power: f64,
    noise_power: f64,
    impulse: Option<ImpulseState>,
    cfo: Option<(Iq, Iq)>,
    cfo_n: u32,
    iq: Option<(f32, f32, f32)>,
    dc: Iq,
    sco: Option<Sco>,
    tmp: Vec<Iq>,
}

impl RadioSim {
    /// Set up the receiver's faults of `ch` for a signal at `fs_hz` whose
    /// carrier has mean power `carrier_power` per sample, against a nominal
    /// channel bandwidth `bandwidth_hz` (8 MHz for the UK).
    pub fn new(ch: &Channel, fs_hz: f64, carrier_power: f64, bandwidth_hz: f64) -> RadioSim {
        let noise_power = ch.cn_db.map_or(0.0, |cn| {
            carrier_power / 10f64.powf(cn / 10.0) * fs_hz / bandwidth_hz
        });
        let impulse = ch.impulse.map(|i| ImpulseState {
            p_start: i.rate_hz / fs_hz,
            width: ((i.width_us * 1e-6 * fs_hz).round() as usize).max(1),
            remaining: 0,
            sigma: (carrier_power * 10f64.powf(i.rel_db / 10.0) / 2.0).sqrt(),
        });
        let cfo = (ch.cfo_hz != 0.0).then(|| {
            let w = TAU * ch.cfo_hz / fs_hz;
            (Iq::new(1.0, 0.0), Iq::new(w.cos() as f32, w.sin() as f32))
        });
        let iq = (ch.iq_gain_db != 0.0 || ch.iq_phase_deg != 0.0).then(|| {
            let phi = ch.iq_phase_deg.to_radians();
            (
                10f64.powf(ch.iq_gain_db / 20.0) as f32,
                phi.sin() as f32,
                phi.cos() as f32,
            )
        });
        let dc_level = (ch.dc_offset * (carrier_power + noise_power).sqrt()) as f32;
        RadioSim {
            rng: Rng::new(ch.seed ^ 0x5DEE_CE66_D1CE_4E5B),
            sigma: (noise_power / 2.0).sqrt(),
            carrier_power,
            noise_power,
            impulse,
            cfo,
            cfo_n: 0,
            iq,
            dc: Iq::new(dc_level, dc_level),
            sco: (ch.sco_ppm != 0.0).then(|| Sco::new(ch.sco_ppm)),
            tmp: Vec::new(),
        }
    }

    /// The mean power of what comes out (carrier plus noise), for setting a
    /// level.
    pub fn output_power(&self) -> f64 {
        self.carrier_power + self.noise_power
    }

    /// The noise power per sample.
    pub fn noise_power(&self) -> f64 {
        self.noise_power
    }

    /// Apply the faults to `x`, appending to `out`.
    pub fn process(&mut self, x: &[Iq], out: &mut Vec<Iq>) {
        self.tmp.clear();
        self.tmp.reserve(x.len());
        for &s in x {
            let mut y = s;
            if self.sigma > 0.0 {
                let (a, b) = self.rng.gaussian_pair();
                y += Iq::new((self.sigma * a) as f32, (self.sigma * b) as f32);
            }
            if let Some(im) = &mut self.impulse {
                if im.remaining == 0 && self.rng.uniform() < im.p_start {
                    im.remaining = im.width;
                }
                if im.remaining > 0 {
                    im.remaining -= 1;
                    let (a, b) = self.rng.gaussian_pair();
                    y += Iq::new((im.sigma * a) as f32, (im.sigma * b) as f32);
                }
            }
            if let Some((ph, step)) = &mut self.cfo {
                y *= *ph;
                *ph *= *step;
                self.cfo_n += 1;
                if self.cfo_n.is_multiple_of(1024) {
                    *ph /= ph.norm();
                }
            }
            if let Some((g, s, c)) = self.iq {
                // Q is scaled and leaks a little of I.
                y = Iq::new(y.re, g * (y.im * c + y.re * s));
            }
            y += self.dc;
            self.tmp.push(y);
        }
        match &mut self.sco {
            Some(s) => s.process(&self.tmp, out),
            None => out.extend_from_slice(&self.tmp),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 64e6 / 7.0;

    fn tone(f_hz: f64, fs: f64, n: usize) -> Vec<Iq> {
        (0..n)
            .map(|k| {
                let p = TAU * f_hz / fs * k as f64;
                Iq::new(p.cos() as f32, p.sin() as f32)
            })
            .collect()
    }

    /// Propagation only.
    fn run(ch: &Channel, fs: f64, x: &[Iq]) -> Vec<Iq> {
        let mut sim = ChannelSim::new(ch, fs, 1.0);
        let mut out = Vec::new();
        sim.process(x, &mut out);
        out
    }

    /// The receiver's faults only, for a unit-power signal.
    fn radio(ch: &Channel, fs: f64, x: &[Iq]) -> Vec<Iq> {
        let mut sim = RadioSim::new(ch, fs, 1.0, 8e6);
        let mut out = Vec::new();
        sim.process(x, &mut out);
        out
    }

    fn power(x: &[Iq]) -> f64 {
        x.iter().map(|c| c.norm_sqr() as f64).sum::<f64>() / x.len() as f64
    }

    #[test]
    fn a_clean_channel_only_delays() {
        let x = tone(1e6, FS, 2000);
        let y = run(&Channel::default(), FS, &x);
        assert_eq!(y.len(), x.len());
        let d = ChannelSim::bulk_delay();
        for n in d..x.len() {
            assert!((y[n] - x[n - d]).norm() < 1e-6, "sample {n}");
        }
    }

    #[test]
    fn blocks_of_any_size_give_the_same_output() {
        let x = tone(1.3e6, FS, 5000);
        let ch = Channel {
            paths: vec![Path::echo(0.0, 0.0), Path::echo(2.37, -4.0)],
            cfo_hz: 500.0,
            cn_db: Some(20.0),
            sco_ppm: 300.0,
            ..Default::default()
        };
        // Both stages, in one go and in blocks of 333.
        let mut chan0 = ChannelSim::new(&ch, FS, 1.0);
        let mut rad0 = RadioSim::new(&ch, FS, chan0.carrier_power(), 8e6);
        let (mut mid0, mut whole) = (Vec::new(), Vec::new());
        chan0.process(&x, &mut mid0);
        rad0.process(&mid0, &mut whole);
        let mut chan = ChannelSim::new(&ch, FS, 1.0);
        let mut rad = RadioSim::new(&ch, FS, chan.carrier_power(), 8e6);
        let (mut mid, mut parts) = (Vec::new(), Vec::new());
        for c in x.chunks(333) {
            mid.clear();
            chan.process(c, &mut mid);
            rad.process(&mid, &mut parts);
        }
        assert_eq!(whole.len(), parts.len());
        assert!(whole.iter().zip(&parts).all(|(a, b)| (a - b).norm() < 1e-5));
    }

    #[test]
    fn a_fractional_delay_shifts_the_phase_by_exactly_that_much() {
        // A 0.5 sample (0.0547 µs) delay on a tone at 0.2 of the rate.
        let f = 0.2 * FS;
        let x = tone(f, FS, 4000);
        let ch = Channel {
            paths: vec![Path::echo(0.5 / FS * 1e6, 0.0)],
            ..Default::default()
        };
        let y = run(&ch, FS, &x);
        let d = ChannelSim::bulk_delay() as f64 + 0.5;
        for (n, got) in y.iter().enumerate().skip(200) {
            let p = TAU * 0.2 * (n as f64 - d);
            let want = Iq::new(p.cos() as f32, p.sin() as f32);
            assert!(
                (got - want).norm() < 2e-3,
                "sample {n}: {got:?} vs {want:?}"
            );
        }
    }

    #[test]
    fn a_zero_db_echo_makes_nulls_where_it_should() {
        // Two equal paths 1 µs apart: |1 + e^{-j2πfτ}|² = 0 at 500 kHz (and
        // odd multiples), 4 at 1 MHz.
        let ch = Channel {
            paths: profile("0db-echo").unwrap(),
            ..Default::default()
        };
        let at = |f: f64| power(&run(&ch, FS, &tone(f, FS, 20_000))[500..]);
        assert!(at(500e3) < 1e-3, "null: {}", at(500e3));
        assert!((at(1e6) - 4.0).abs() < 0.05, "peak: {}", at(1e6));
        assert!(at(1.5e6) < 1e-3, "null: {}", at(1.5e6));
    }

    #[test]
    fn noise_is_set_against_the_nominal_bandwidth() {
        // C/N 10 dB, unit signal: noise over the whole sample rate is
        // 0.1 · fs / 8 MHz.
        let ch = Channel {
            cn_db: Some(10.0),
            ..Default::default()
        };
        let y = radio(&ch, FS, &vec![Iq::new(0.0, 0.0); 400_000]);
        let want = 0.1 * FS / 8e6;
        assert!(
            (power(&y) / want - 1.0).abs() < 0.02,
            "{} vs {want}",
            power(&y)
        );
        // And reports it.
        assert!((RadioSim::new(&ch, FS, 1.0, 8e6).noise_power() - want).abs() < 1e-9);
    }

    #[test]
    fn a_frequency_offset_moves_the_tone() {
        let x = tone(1e6, FS, 4000);
        let ch = Channel {
            cfo_hz: 12_345.0,
            ..Default::default()
        };
        let y = radio(&ch, FS, &x);
        let step: f64 = y[100..]
            .windows(2)
            .map(|w| (w[1] * w[0].conj()).arg() as f64)
            .sum::<f64>()
            / (y.len() - 101) as f64;
        assert!(
            (step * FS / TAU - 1_012_345.0).abs() < 5.0,
            "{}",
            step * FS / TAU
        );
    }

    #[test]
    fn a_clock_offset_scales_the_frequency() {
        let x = tone(1e6, FS, 40_000);
        let ch = Channel {
            sco_ppm: 2000.0,
            ..Default::default()
        };
        let y = radio(&ch, FS, &x);
        // The receiver's clock is 0.2 % fast: it takes samples closer
        // together, so a tone has fewer cycles between them and shows at
        // f / (1 + ppm).
        let step: f64 = y[100..]
            .windows(2)
            .map(|w| (w[1] * w[0].conj()).arg() as f64)
            .sum::<f64>()
            / (y.len() - 101) as f64;
        let want = 1e6 / 1.002;
        assert!(
            (step * FS / TAU - want).abs() < want * 1e-4,
            "{}",
            step * FS / TAU
        );
        // And a little more samples out than in: 0.2 % more in the time.
        assert!(y.len() > x.len());
        assert!((y.len() as f64 / x.len() as f64 - 1.002).abs() < 1e-3);
    }

    #[test]
    fn rayleigh_fading_has_unit_mean_power_and_deep_fades() {
        // A slow rate so that a short run holds many fades: fd = 100 Hz at
        // 100 kHz, a coherence time of ~100 samples.
        let fs = 100e3;
        let ch = Channel {
            paths: vec![Path {
                fading: Fading::Rayleigh {
                    max_doppler_hz: 100.0,
                },
                ..Path::echo(0.0, 0.0)
            }],
            ..Default::default()
        };
        let y = run(&ch, fs, &vec![Iq::new(1.0, 0.0); 400_000]);
        let y = &y[100..];
        assert!((power(y) - 1.0).abs() < 0.12, "{}", power(y));
        // Rayleigh: the power is below −10 dB (0.1) about 10 % of the time.
        let deep = y.iter().filter(|c| c.norm_sqr() < 0.1).count() as f64 / y.len() as f64;
        assert!((deep - 0.095).abs() < 0.04, "{deep}");
    }

    #[test]
    fn doppler_shifts_a_path() {
        let x = tone(1e6, FS, 4000);
        let ch = Channel {
            paths: vec![Path {
                doppler_hz: 300.0,
                ..Path::echo(0.0, 0.0)
            }],
            ..Default::default()
        };
        let y = run(&ch, FS, &x);
        let step: f64 = y[100..]
            .windows(2)
            .map(|w| (w[1] * w[0].conj()).arg() as f64)
            .sum::<f64>()
            / (y.len() - 101) as f64;
        assert!((step * FS / TAU - 1_000_300.0).abs() < 5.0);
    }

    #[test]
    fn impulses_come_at_about_the_asked_rate() {
        let ch = Channel {
            impulse: Some(Impulse {
                rate_hz: 1000.0,
                rel_db: 10.0,
                width_us: 5.0,
            }),
            ..Default::default()
        };
        // Silence in, one second of it at 1 MS/s: bursts of ~5 samples.
        let y = radio(&ch, 1e6, &vec![Iq::new(0.0, 0.0); 1_000_000]);
        let active = y.iter().filter(|c| c.norm_sqr() > 0.0).count();
        // ~1000 bursts a second × 5 samples (less the overlaps).
        assert!((4000..6000).contains(&active), "{active}");
    }

    #[test]
    fn dc_and_iq_faults_show_up() {
        let ch = Channel {
            dc_offset: 0.1,
            ..Default::default()
        };
        let y = radio(&ch, FS, &vec![Iq::new(0.0, 0.0); 100]);
        assert!((y[50].re - 0.1).abs() < 1e-6 && (y[50].im - 0.1).abs() < 1e-6);
        let ch = Channel {
            iq_gain_db: 6.0206,
            ..Default::default()
        };
        let y = radio(&ch, FS, &[Iq::new(1.0, 1.0); 100]);
        assert!((y[50].im - 2.0).abs() < 1e-3 && (y[50].re - 1.0).abs() < 1e-3);
    }

    #[test]
    fn paths_parse_as_typed() {
        assert_eq!(Path::parse("2.5:-3").unwrap(), Path::echo(2.5, -3.0));
        let p = Path::parse("1:0:90:50").unwrap();
        assert_eq!((p.phase_deg, p.doppler_hz), (90.0, 50.0));
        assert!(Path::parse("1").is_err());
        assert!(Path::parse("a:b").is_err());
        assert!(Path::parse("-1:0").is_err());
        assert!(profile("tu6").unwrap().len() == 6);
        assert!(profile("nope").is_none());
    }
}
