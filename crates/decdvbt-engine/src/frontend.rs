// made by CasualArclamp
//! Corrections to the radio's samples before anything else sees them: the DC
//! offset a direct-conversion receiver leaves at the centre of the band, and
//! the imbalance between its I and Q paths that puts a faint mirror image of
//! the signal across the centre.
//!
//! Both are estimated from a strided look at every block (one sample in
//! eight: the signal is noise-like, so a few thousand samples give the
//! figures to a hundredth of a percent, and the cost is nothing beside the
//! pass that applies them) and followed slowly, over a tenth of a second, so
//! the signal's own fluctuations average out of them.
//!
//! - **DC**: the mean of the samples. A DVB-T carrier sits at the centre of the
//!   band when the radio is tuned to the channel, and a constant added to
//!   every sample lands on it. The mean of the signal itself is zero over any
//!   stretch of many symbols, so taking the estimate off loses nothing.
//! - **I/Q balance**, blind, from the signal's circularity: OFDM is noise-like
//!   and `E[s²] = 0`, so any `E[x²]` the radio's samples show is imbalance.
//!   With `x = s + b·s*` (the image `b·s*` is the imbalance), `E[x²]/E[|x|²]`
//!   is `2b` to first order, and `y = x − b·x*` takes the image out.

use decdvbt_core::Iq;

/// One sample in this many is looked at to make the estimates.
const STRIDE: usize = 8;
/// How much of each block's reading goes into the running estimate: the time
/// constant is about 85 ms at the blocks the radio hands over (13 ms).
const DC_RATE: f32 = 0.15;
const IQ_RATE: f32 = 0.08;

/// Which corrections are made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrontEndSettings {
    pub dc_removal: bool,
    pub iq_balance: bool,
}

impl Default for FrontEndSettings {
    fn default() -> Self {
        // The mean costs nothing to take off; the image correction is a
        // second pass over every sample and is for a radio whose image shows.
        FrontEndSettings {
            dc_removal: true,
            iq_balance: false,
        }
    }
}

/// What the estimates say, whether or not they are being applied.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrontEndReading {
    /// The DC offset's magnitude against full scale, dB.
    pub dc_dbfs: f32,
    /// How far the image is under the signal, dB (80 for none to be seen).
    pub image_rejection_db: f32,
}

impl Default for FrontEndReading {
    fn default() -> Self {
        FrontEndReading {
            dc_dbfs: -120.0,
            image_rejection_db: 80.0,
        }
    }
}

pub struct FrontEnd {
    pub settings: FrontEndSettings,
    dc: Iq,
    /// The image's coefficient b.
    b: Iq,
    primed: bool,
}

impl FrontEnd {
    pub fn new(settings: FrontEndSettings) -> FrontEnd {
        FrontEnd {
            settings,
            dc: Iq::new(0.0, 0.0),
            b: Iq::new(0.0, 0.0),
            primed: false,
        }
    }

    pub fn reading(&self) -> FrontEndReading {
        FrontEndReading {
            dc_dbfs: 20.0 * self.dc.norm().max(1e-6).log10(),
            image_rejection_db: (-20.0 * self.b.norm().max(1e-4).log10()).min(80.0),
        }
    }

    /// Follow the block's DC offset and imbalance, and take them off it as
    /// the settings say.
    pub fn apply(&mut self, block: &mut [Iq]) {
        let looked: Vec<Iq> = block.iter().step_by(STRIDE).copied().collect();
        if looked.is_empty() {
            return;
        }
        let n = looked.len() as f32;
        let mean = looked.iter().sum::<Iq>() / n;
        // The circularity of what is left once the mean is out.
        let (mut e2, mut power) = (Iq::new(0.0, 0.0), 0.0f32);
        for x in &looked {
            let y = x - mean;
            e2 += y * y;
            power += y.norm_sqr();
        }
        let b = if power > 0.0 {
            e2 / (2.0 * power)
        } else {
            Iq::new(0.0, 0.0)
        };
        if self.primed {
            self.dc += (mean - self.dc) * DC_RATE;
            self.b += (b - self.b) * IQ_RATE;
        } else {
            (self.dc, self.b, self.primed) = (mean, b, true);
        }
        let (dc, b) = (self.dc, self.b);
        match (self.settings.dc_removal, self.settings.iq_balance) {
            (false, false) => {}
            (true, false) => {
                for x in block {
                    *x -= dc;
                }
            }
            (dc_on, true) => {
                let off = if dc_on { dc } else { Iq::new(0.0, 0.0) };
                for x in block {
                    let y = *x - off;
                    *x = y - b * y.conj();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Complex noise of unit power from a small generator (sum of uniforms).
    fn noise(n: usize, seed: &mut u64) -> Vec<Iq> {
        let mut u = || {
            *seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((*seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5
        };
        (0..n)
            .map(|_| {
                let g = |u: &mut dyn FnMut() -> f32| (0..12).map(|_| u()).sum::<f32>();
                Iq::new(g(&mut u), g(&mut u)) * std::f32::consts::FRAC_1_SQRT_2
            })
            .collect()
    }

    fn radio(n: usize, seed: &mut u64) -> Vec<Iq> {
        // A DC offset, and an image 30 dB down (b = 0.0316) at some phase.
        let b = Iq::from_polar(0.0316, 0.7);
        noise(n, seed)
            .into_iter()
            .map(|s| 0.1 * (s + b * s.conj()) + Iq::new(0.02, -0.01))
            .collect()
    }

    #[test]
    fn the_dc_offset_is_followed_and_taken_off() {
        let mut seed = 7;
        let mut fe = FrontEnd::new(FrontEndSettings::default());
        let mut last = Vec::new();
        for _ in 0..40 {
            last = radio(131_072, &mut seed);
            fe.apply(&mut last);
        }
        let mean = last.iter().sum::<Iq>() / last.len() as f32;
        assert!(mean.norm() < 5e-4, "{mean}");
        let r = fe.reading();
        // 0.0224 of full scale.
        assert!(
            (r.dc_dbfs - 20.0 * 0.0224f32.log10()).abs() < 0.5,
            "{}",
            r.dc_dbfs
        );
    }

    #[test]
    fn the_image_is_read_and_taken_out_when_asked() {
        let circularity = |x: &[Iq]| {
            let m = x.iter().sum::<Iq>() / x.len() as f32;
            let (mut e2, mut p) = (Iq::new(0.0, 0.0), 0.0f32);
            for s in x {
                let y = s - m;
                e2 += y * y;
                p += y.norm_sqr();
            }
            (e2 / p).norm() / 2.0
        };
        let mut seed = 11;
        let mut fe = FrontEnd::new(FrontEndSettings {
            dc_removal: true,
            iq_balance: true,
        });
        let mut last = Vec::new();
        for _ in 0..60 {
            last = radio(131_072, &mut seed);
            fe.apply(&mut last);
        }
        // Read: 30 dB (0.0316).
        let r = fe.reading();
        assert!(
            (r.image_rejection_db - 30.0).abs() < 1.5,
            "{}",
            r.image_rejection_db
        );
        // Taken out: from 0.0316 to under 0.004.
        assert!(circularity(&last) < 0.004, "{}", circularity(&last));
        // Off: read but not changed.
        let mut off = FrontEnd::new(FrontEndSettings {
            dc_removal: false,
            iq_balance: false,
        });
        let mut x = radio(131_072, &mut seed);
        let before = x.clone();
        off.apply(&mut x);
        assert_eq!(x, before);
        assert!((off.reading().image_rejection_db - 30.0).abs() < 2.5);
    }
}
