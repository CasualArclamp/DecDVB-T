// made by CasualArclamp
//! A streaming rational resampler for complex samples: interpolate by `up`,
//! low-pass, decimate by `down`, done as a polyphase filter so only the
//! samples kept are computed.
//!
//! For DVB-T the input is 64/7 MS/s (one sample a T) and the output a
//! radio's rate: 10 MS/s is 35/32, 20 MS/s is 35/16.

use decdvbt_core::Iq;

/// The zeroth-order modified Bessel function, for the Kaiser window.
pub fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..50 {
        term *= (x / (2.0 * k as f64)).powi(2);
        sum += term;
        if term < 1e-14 * sum {
            break;
        }
    }
    sum
}

/// A Kaiser-windowed sinc low-pass: `taps` coefficients, cutoff `fc` (a
/// fraction of the sample rate it runs at), window shape `beta`.
pub fn kaiser_lowpass(taps: usize, fc: f64, beta: f64) -> Vec<f32> {
    let m = (taps - 1) as f64 / 2.0;
    let i0b = bessel_i0(beta);
    (0..taps)
        .map(|n| {
            let t = n as f64 - m;
            let sinc = if t == 0.0 {
                2.0 * fc
            } else {
                (2.0 * std::f64::consts::PI * fc * t).sin() / (std::f64::consts::PI * t)
            };
            let r = t / m;
            let w = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b;
            (sinc * w) as f32
        })
        .collect()
}

/// The ratio up/down in lowest terms that gets `out_hz / in_hz` to a part in a
/// million, with `up` at most `max_up`.
pub fn rational_ratio(in_hz: f64, out_hz: f64, max_up: usize) -> Option<(usize, usize)> {
    let r = out_hz / in_hz;
    // out/in = up/down, so for each `up` the `down` that gets nearest.
    (1..=max_up).find_map(|up| {
        let down = (up as f64 / r).round();
        (down >= 1.0 && ((up as f64 / down) - r).abs() < 1e-6 * r).then_some((up, down as usize))
    })
}

pub struct Resampler {
    up: usize,
    down: usize,
    taps_per_phase: usize,
    /// Polyphase branches: `phases[p][k]` = h[p + k·up].
    phases: Vec<Vec<f32>>,
    /// The most recent inputs, newest last.
    hist: Vec<Iq>,
    /// Position of the next output, in units of 1/up input samples, counted
    /// from the first sample of `hist`'s newest input.
    next: usize,
}

impl Resampler {
    /// Resample by `up`/`down` for an input at `in_hz`: pass up to
    /// `cutoff_hz`, with a transition `transition_hz` wide, and about 70 dB
    /// of rejection beyond it.
    pub fn new(
        up: usize,
        down: usize,
        in_hz: f64,
        cutoff_hz: f64,
        transition_hz: f64,
    ) -> Resampler {
        assert!(up >= 1 && down >= 1);
        // The prototype runs at F = in_hz · up; Kaiser's length formula.
        let f = in_hz * up as f64;
        let (fc, width) = (cutoff_hz / f, transition_hz / f);
        let atten = 70.0;
        let beta = 0.1102 * (atten - 8.7);
        let len = (((atten - 7.95) / (14.36 * width)).ceil() as usize).max(8);
        let taps_per_phase = len.div_ceil(up);
        let total = taps_per_phase * up;
        // A symmetric prototype with the interpolation's gain, split into its
        // `up` polyphase branches.
        let mut h = kaiser_lowpass(total | 1, fc.min(0.499), beta);
        h.resize(total, 0.0);
        for c in &mut h {
            *c *= up as f32;
        }
        let phases = (0..up)
            .map(|p| (0..taps_per_phase).map(|k| h[p + k * up]).collect())
            .collect();
        Resampler {
            up,
            down,
            taps_per_phase,
            phases,
            hist: vec![Iq::new(0.0, 0.0); taps_per_phase],
            next: 0,
        }
    }

    /// From `in_hz` to `out_hz` for a signal `bandwidth_hz` wide (centred on
    /// zero). The stopband starts at the first thing that would alias or
    /// image into the band: min(in, out) less the band's edge.
    pub fn for_rates(in_hz: f64, out_hz: f64, bandwidth_hz: f64) -> Result<Resampler, String> {
        let (up, down) = rational_ratio(in_hz, out_hz, 256)
            .ok_or_else(|| format!("no simple ratio from {in_hz} to {out_hz} Hz"))?;
        let edge = bandwidth_hz / 2.0;
        let stop = in_hz.min(out_hz) - edge;
        if stop <= edge * 1.02 {
            return Err(format!(
                "a {:.2} MHz signal does not fit a {:.2} MS/s output",
                bandwidth_hz / 1e6,
                out_hz / 1e6
            ));
        }
        Ok(Resampler::new(
            up,
            down,
            in_hz,
            (edge + stop) / 2.0,
            stop - edge,
        ))
    }

    pub fn ratio(&self) -> (usize, usize) {
        (self.up, self.down)
    }

    /// Resample `input`, appending to `out`.
    pub fn process(&mut self, input: &[Iq], out: &mut Vec<Iq>) {
        for &x in input {
            // `hist` holds the newest `taps_per_phase` inputs, x[n] last.
            self.hist.rotate_left(1);
            *self.hist.last_mut().unwrap() = x;
            // Outputs due while this input is the newest: those whose time
            // falls in [n, n + 1), i.e. `next` < up.
            while self.next < self.up {
                let phase = &self.phases[self.next];
                let mut acc = Iq::new(0.0, 0.0);
                for (k, &c) in phase.iter().enumerate() {
                    acc += self.hist[self.taps_per_phase - 1 - k] * c;
                }
                out.push(acc);
                self.next += self.down;
            }
            self.next -= self.up;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    fn tone(f: f64, n: usize) -> Vec<Iq> {
        (0..n)
            .map(|k| {
                let p = TAU * f * k as f64;
                Iq::new(p.cos() as f32, p.sin() as f32)
            })
            .collect()
    }

    fn power(x: &[Iq]) -> f64 {
        x.iter().map(|c| c.norm_sqr() as f64).sum::<f64>() / x.len() as f64
    }

    #[test]
    fn the_dvbt_to_hackrf_ratios_are_found() {
        let t = 64e6 / 7.0;
        assert_eq!(rational_ratio(t, 10e6, 256), Some((35, 32)));
        assert_eq!(rational_ratio(t, 20e6, 256), Some((35, 16)));
        assert_eq!(rational_ratio(10e6, 5e6, 256), Some((1, 2)));
    }

    #[test]
    fn an_in_band_tone_keeps_its_power_and_its_frequency() {
        // In at 64/7 MS/s, out at 10: a tone at 2 MHz is 0.2187 of the
        // input rate and 0.2 of the output's.
        let t = 64e6 / 7.0;
        let mut r = Resampler::for_rates(t, 10e6, 7.61e6).unwrap();
        assert_eq!(r.ratio(), (35, 32));
        let x = tone(2e6 / t, 20_000);
        let mut y = Vec::new();
        r.process(&x, &mut y);
        // 20000 · 35/32 samples.
        assert!((y.len() as i64 - 21_875).abs() <= 2, "{}", y.len());
        let settled = &y[500..];
        assert!((power(settled) - 1.0).abs() < 0.02, "{}", power(settled));
        // Its frequency: the phase step between samples, 0.2 cycles.
        let step: f64 = settled
            .windows(2)
            .map(|w| (w[1] * w[0].conj()).arg() as f64)
            .sum::<f64>()
            / (settled.len() - 1) as f64;
        assert!((step / TAU - 0.2).abs() < 1e-4, "{}", step / TAU);
    }

    /// The power of `y` at `f` (a fraction of its sample rate): one DFT bin.
    fn power_at(y: &[Iq], f: f64) -> f64 {
        let mut acc = (0.0f64, 0.0f64);
        for (n, c) in y.iter().enumerate() {
            let p = -TAU * f * n as f64;
            acc.0 += c.re as f64 * p.cos() - c.im as f64 * p.sin();
            acc.1 += c.re as f64 * p.sin() + c.im as f64 * p.cos();
        }
        (acc.0 * acc.0 + acc.1 * acc.1) / (y.len() * y.len()) as f64
    }

    #[test]
    fn the_band_edge_passes_and_the_images_are_rejected() {
        let t = 64e6 / 7.0;
        // A tone at the band's edge (3.8 MHz) comes through at nearly full
        // power.
        let mut r = Resampler::for_rates(t, 10e6, 7.61e6).unwrap();
        let mut y = Vec::new();
        r.process(&tone(3.8e6 / t, 20_000), &mut y);
        assert!(power(&y[500..]) > 0.9, "{}", power(&y[500..]));
        // A 1 MHz tone has an image about the input rate at 8.14 MHz, which
        // at 10 MS/s folds to 1.86 MHz: it must be gone.
        let mut r = Resampler::for_rates(t, 10e6, 7.61e6).unwrap();
        let mut y = Vec::new();
        r.process(&tone(1e6 / t, 40_000), &mut y);
        let y = &y[1000..];
        let wanted = power_at(y, 0.1);
        let image = power_at(y, 0.186);
        assert!(wanted > 0.9, "{wanted}");
        assert!(image < wanted * 1e-6, "image {image} vs {wanted}");
    }

    #[test]
    fn it_refuses_a_signal_wider_than_the_output() {
        assert!(Resampler::for_rates(64e6 / 7.0, 4e6, 7.61e6).is_err());
    }

    #[test]
    fn blocks_can_be_any_size() {
        let t = 64e6 / 7.0;
        let x = tone(1e6 / t, 5_000);
        let mut whole = Vec::new();
        Resampler::for_rates(t, 10e6, 7.61e6)
            .unwrap()
            .process(&x, &mut whole);
        let mut r = Resampler::for_rates(t, 10e6, 7.61e6).unwrap();
        let mut parts = Vec::new();
        for c in x.chunks(777) {
            r.process(c, &mut parts);
        }
        assert_eq!(whole.len(), parts.len());
        assert!(whole.iter().zip(&parts).all(|(a, b)| (a - b).norm() < 1e-6));
    }
}
