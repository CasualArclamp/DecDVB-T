// made by CasualArclamp
//! Host-side real-to-IQ conversion for the Airspy R2.
//!
//! The R2's ADC runs at 20 MS/s and the R820T presents the channel on a 5 MHz
//! IF (fs/4), so the bulk stream is a *real* signal. Turning it into 10 MS/s
//! complex baseband follows libairspy's `iqconverter_float.c` (BSD-3-Clause,
//! airspyone_host): remove the DC, translate by fs/4 with the sign pattern
//! (-1, -hbc, +1, +hbc), half-band filter, decimate by 2.
//!
//! The fs/4 translate makes the even samples the real parts and the odd
//! samples the imaginary parts of a complex stream at 20 MS/s. A half-band
//! filter has zero taps at every second position, so a decimate-by-2 output
//! needs only: the even lane through 16 non-zero taps (the real part) and the
//! odd lane delayed to the filter's group delay times the centre tap (the
//! imaginary part). That is the whole cost: 8 multiplies (the taps are
//! symmetric) per output sample.
//!
//! # Spectral orientation — **needs a live check**
//!
//! [`IF_SPECTRUM_INVERTED`] is the one place that decides whether the band
//! comes out mirrored. libairspy's sign pattern equals mixing with
//! `-exp(+j*pi*n/2)`, which presents an RF tone at *centre + d* as a real IF
//! tone at *5 MHz - d* (the R820T's low-side injection), and puts it at
//! *+d* in the output. If the real radio is mirrored, set the constant to
//! `false`: that conjugates the output (negates Q).
//!
//! # Level
//!
//! Output is scaled so that a full-scale real tone (ADC 0..4095 swinging
//! +-2048 about the midpoint) gives a complex tone of magnitude 1.0, I and Q
//! each peaking at +-1: the same full scale as the HackRF's cs8 (127/128), so
//! the receiver's level and clip logic read the same. (libairspy's own output
//! is half of this; its callers rescale.)

use num_complex::Complex32;

/// See the module comment. `true`: the band is as libairspy delivers it.
pub const IF_SPECTRUM_INVERTED: bool = true;

/// Q is negated when the assumption is flipped (conjugation = mirror).
const Q_SIGN: f32 = if IF_SPECTRUM_INVERTED { 1.0 } else { -1.0 };

/// The 16 non-zero taps of the 31-tap half-band h[0], h[2], … h[30]
/// (symmetric; equiripple; at 20 MS/s flat to 3.9 MHz within 0.010 dB and more
/// than 58 dB down from 6.1 MHz, measured by `half_band_response`). With the
/// centre tap h[15] they sum to 0.99963.
// The digits are the design's (f64); the f32 rounding is deliberate.
#[allow(clippy::excessive_precision)]
pub const HALF_BAND: [f32; 16] = [
    -0.001_471_609,
    0.003_776_939,
    -0.008_221_535,
    0.015_790_741,
    -0.028_336_383,
    0.050_286_030,
    -0.097_619_457,
    0.315_607_795,
    0.315_607_795,
    -0.097_619_457,
    0.050_286_030,
    -0.028_336_383,
    0.015_790_741,
    -0.008_221_535,
    0.003_776_939,
    -0.001_471_609,
];
/// The half-band's centre tap h[15]: it multiplies the odd lane.
pub const CENTRE_TAP: f32 = 0.5;

/// Real samples handled per pass: small enough that the lanes stay in L1.
const BLOCK: usize = 4096;
/// Complex outputs per block.
const PAIRS: usize = BLOCK / 2;
/// Even-lane history the 16-tap filter needs.
const EV_HIST: usize = 15;
/// Odd-lane delay, in pairs: the filter's group delay (15.5 samples at
/// 20 MS/s seen from the even lane = 8 pairs from the odd lane's position).
const OD_DELAY: usize = 8;
/// 12-bit unsigned offset-binary, midpoint 2048.
const MID: f32 = 2048.0;
/// Counts to unit scale, x2 for the level in the module comment.
const SCALE: f32 = 2.0 / MID;
/// How fast the DC estimate follows the block mean (per block of 4096).
const DC_ALPHA: f32 = 0.05;

/// The converter. All buffers are fixed-size and owned here, so a block costs
/// no allocation.
pub struct RealToIq {
    /// The tracked mean of the raw samples, ADC counts.
    dc: f32,
    primed: bool,
    // Rust note: big arrays live in a `Box` so the struct moves cheaply and
    // never overflows a thread's stack.
    ev: Box<[f32; EV_HIST + PAIRS]>,
    od: Box<[f32; OD_DELAY + PAIRS]>,
    acc: Box<[f32; PAIRS]>,
    raw: Box<[u16; BLOCK]>,
    blanked: u64,
}

impl Default for RealToIq {
    fn default() -> Self {
        Self::new()
    }
}

impl RealToIq {
    pub fn new() -> Self {
        RealToIq {
            dc: MID,
            primed: false,
            ev: Box::new([0.0; EV_HIST + PAIRS]),
            od: Box::new([0.0; OD_DELAY + PAIRS]),
            acc: Box::new([0.0; PAIRS]),
            raw: Box::new([0; BLOCK]),
            blanked: 0,
        }
    }

    /// Convert unpacked samples (little-endian u16 each) and append the
    /// complex output to `out`. Only whole groups of 4 samples (8 bytes) are
    /// used: the sign pattern repeats every 4 samples, so a transfer must not
    /// end inside one. Returns the bytes consumed.
    pub fn process_unpacked(&mut self, bytes: &[u8], out: &mut Vec<Complex32>) -> usize {
        let usable = bytes.len() / 8 * 8;
        out.reserve(usable / 4);
        for chunk in bytes[..usable].chunks(BLOCK * 2) {
            let n = chunk.len() / 2;
            let (words, _) = chunk.as_chunks::<2>();
            for (r, w) in self.raw[..n].iter_mut().zip(words) {
                *r = u16::from_le_bytes(*w) & 0x0fff;
            }
            self.block(n, out);
        }
        usable
    }

    /// As [`process_unpacked`](Self::process_unpacked) for the packed format
    /// (8 samples in 12 bytes). Returns the bytes consumed.
    ///
    /// The R2 sometimes sends a stretch of a packed transfer still in the
    /// unpacked format (seen live at 10 MS/s: about every 2 s, bytes
    /// 512..12288 of a transfer, i.e. the first 12 KB chunk, as u16 words;
    /// presumably the firmware's in-place packer had not finished it). Read
    /// as packed that is near full-scale noise, +8 dB on the buffer, and the
    /// receiver loses lock. Such a block is found by [`looks_unpacked`] and
    /// replaced by the DC level, i.e. silence: the sample count and so the
    /// receiver's timing stay right. [`blanked`](Self::blanked) counts them.
    pub fn process_packed(&mut self, bytes: &[u8], out: &mut Vec<Complex32>) -> usize {
        let usable = bytes.len() / 12 * 12;
        out.reserve(usable / 6);
        // 8 samples per 12 bytes: a block of 4096 samples is 6144 bytes.
        for chunk in bytes[..usable].chunks(BLOCK / 8 * 12) {
            let n = chunk.len() / 12 * 8;
            if looks_unpacked(chunk) {
                // The block's mean is then the DC estimate itself, so the
                // tracker does not move either.
                let dc = self.dc.round() as u16;
                self.raw[..n].fill(dc);
                self.blanked += 1;
            } else {
                unpack_12bit(chunk, &mut self.raw[..n]);
            }
            self.block(n, out);
        }
        usable
    }

    /// Packed blocks blanked because they arrived unpacked (see
    /// [`process_packed`](Self::process_packed)), since the start.
    pub fn blanked(&self) -> u64 {
        self.blanked
    }

    /// One block of `n` raw samples (a multiple of 4, at most `BLOCK`) from
    /// `self.raw`.
    fn block(&mut self, n: usize, out: &mut Vec<Complex32>) {
        debug_assert!(n.is_multiple_of(4) && n <= BLOCK);
        if n == 0 {
            return;
        }
        let m = n / 2;

        // DC: the block mean, followed slowly (libairspy runs a one-pole
        // filter per sample; that recursion cannot be vectorised, and the
        // offset is steady). At fs/4 translation the DC would land at 5 MHz,
        // on the half-band's transition, so it must come out first.
        // The sum is exact in a u32: 4096 * 4095 < 2^24.
        let sum: u32 = self.raw[..n].iter().map(|&r| r as u32).sum();
        let mean = sum as f32 / n as f32;
        if self.primed {
            self.dc += DC_ALPHA * (mean - self.dc);
        } else {
            self.dc = mean;
            self.primed = true;
        }
        let dc = self.dc;

        // Split into lanes with the translate's signs: samples 0..3 of each
        // group get (-1, -1, +1, +1); the odd lane's hbc factor is applied
        // as CENTRE_TAP at the output.
        {
            let ev = &mut self.ev[EV_HIST..EV_HIST + m];
            let od = &mut self.od[OD_DELAY..OD_DELAY + m];
            // `as_chunks` views the slice as `&[[T; N]]` (the length is in the
            // type, so no bounds checks inside the loop); `m` and `n` are
            // multiples of the chunk size, so no remainder is left over.
            let (groups, _) = self.raw[..n].as_chunks::<4>();
            let (ev, _) = ev.as_chunks_mut::<2>();
            let (od, _) = od.as_chunks_mut::<2>();
            for ((g, e), o) in groups.iter().zip(ev.iter_mut()).zip(od.iter_mut()) {
                e[0] = -(g[0] as f32 - dc) * SCALE;
                o[0] = -(g[1] as f32 - dc) * SCALE;
                e[1] = (g[2] as f32 - dc) * SCALE;
                o[1] = (g[3] as f32 - dc) * SCALE;
            }
        }

        // Real part: symmetric 16-tap FIR over the even lane, `ev[j..j+16]`
        // for output j (the history sits in front). Written as eight passes
        // of `acc += g * (a + b)` over slices, which LLVM turns into SIMD.
        let ev = &self.ev[..EV_HIST + m];
        let acc = &mut self.acc[..m];
        acc.fill(0.0);
        for i in 0..8 {
            let g = HALF_BAND[i];
            let a = &ev[i..i + m];
            let b = &ev[15 - i..15 - i + m];
            for ((s, &x), &y) in acc.iter_mut().zip(a).zip(b) {
                *s += g * (x + y);
            }
        }

        // Imaginary part: the odd lane, delayed OD_DELAY pairs, times the
        // centre tap.
        let q = &self.od[..m];
        out.extend(
            acc.iter()
                .zip(q)
                .map(|(&re, &im)| Complex32::new(re, Q_SIGN * CENTRE_TAP * im)),
        );

        // Keep the tail of each lane as the next block's history.
        self.ev.copy_within(m..m + EV_HIST, 0);
        self.od.copy_within(m..m + OD_DELAY, 0);
    }

    /// Start again from silence (after a retune the old state is stale).
    /// The blanked count carries on.
    pub fn reset(&mut self) {
        self.ev.fill(0.0);
        self.od.fill(0.0);
    }
}

/// Pieces a packed block is checked in: 384 bytes, 192 u16 words.
const CHECK: usize = 384;

/// Whether any 384-byte piece of a packed block holds unpacked samples:
/// read as u16 words, unpacked 12-bit samples all have a zero top nibble,
/// while packed bytes give one about 1 time in 16 (a quiet input around the
/// 2048 midpoint, 1 in 6 at most). Half the words is far from both.
pub fn looks_unpacked(bytes: &[u8]) -> bool {
    bytes.as_chunks::<CHECK>().0.iter().any(|piece| {
        let (words, _) = piece.as_chunks::<2>();
        // Counted with `sum` over a bool->u32 map, which LLVM vectorises.
        let zero: u32 = words.iter().map(|w| (w[1] < 0x10) as u32).sum();
        zero as usize * 2 > CHECK / 2
    })
}

/// Unpack the Airspy's packed 12-bit format: 8 samples per 12 bytes, three
/// little-endian u32 words (libairspy `airspy_unpack`, i.e. `unpack_samples`
/// in `airspy.c`). `out.len()` must be `8 * bytes.len() / 12`.
pub fn unpack_12bit(bytes: &[u8], out: &mut [u16]) {
    let (words, _) = bytes.as_chunks::<12>();
    let (outs, _) = out.as_chunks_mut::<8>();
    for (w, o) in words.iter().zip(outs.iter_mut()) {
        let w0 = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
        let w1 = u32::from_le_bytes([w[4], w[5], w[6], w[7]]);
        let w2 = u32::from_le_bytes([w[8], w[9], w[10], w[11]]);
        o[0] = ((w0 >> 20) & 0xfff) as u16;
        o[1] = ((w0 >> 8) & 0xfff) as u16;
        o[2] = (((w0 & 0xff) << 4) | ((w1 >> 28) & 0xf)) as u16;
        o[3] = ((w1 & 0x0fff_0000) >> 16) as u16;
        o[4] = ((w1 & 0xfff0) >> 4) as u16;
        o[5] = (((w1 & 0xf) << 8) | ((w2 & 0xff00_0000) >> 24)) as u16;
        o[6] = ((w2 >> 12) & 0xfff) as u16;
        o[7] = (w2 & 0xfff) as u16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustfft::{FftPlanner, num_complex::Complex};
    use std::f64::consts::PI;

    /// Little-endian u16 bytes of a real signal, as the Airspy streams it.
    fn adc_bytes(x: &[f64]) -> Vec<u8> {
        x.iter()
            .flat_map(|&v| ((2048.0 + 2047.0 * v).round().clamp(0.0, 4095.0) as u16).to_le_bytes())
            .collect()
    }

    /// A real tone of `amp` (1.0 = full scale) at `hz`, 20 MS/s.
    fn tone(hz: f64, amp: f64, n: usize) -> Vec<f64> {
        (0..n)
            .map(|k| amp * (2.0 * PI * hz * k as f64 / 20e6 + 0.7).cos())
            .collect()
    }

    /// The strongest bin of `z` (10 MS/s), as a frequency in Hz, plus its
    /// magnitude relative to a unit tone.
    fn peak(z: &[Complex32]) -> (f64, f64) {
        let n = z.len().next_power_of_two() / 2;
        let z = &z[z.len() - n..];
        let mut buf: Vec<Complex<f64>> = z
            .iter()
            .enumerate()
            .map(|(k, c)| {
                // A Hann window keeps the leakage of a tone off its neighbours.
                let w = 0.5 - 0.5 * (2.0 * PI * k as f64 / n as f64).cos();
                Complex::new(c.re as f64 * w, c.im as f64 * w)
            })
            .collect();
        FftPlanner::new().plan_fft_forward(n).process(&mut buf);
        let (k, m) = buf
            .iter()
            .enumerate()
            .map(|(k, c)| (k, c.norm()))
            .fold((0, 0.0), |a, b| if b.1 > a.1 { b } else { a });
        let k = if k > n / 2 {
            k as f64 - n as f64
        } else {
            k as f64
        };
        (k * 10e6 / n as f64, m / (n as f64 * 0.5))
    }

    /// `hz` moved onto an FFT bin of `peak` for 2^16 real samples, so a tone
    /// there is not scalloped.
    fn on_bin(hz: f64) -> f64 {
        let spacing = 10e6 / 16384.0;
        (hz / spacing).round() * spacing
    }

    /// Where the radio puts an RF tone `d` Hz off the tuned centre: on the
    /// 5 MHz IF, below it when the spectrum is inverted (libairspy's case).
    fn rf_to_if(d: f64) -> f64 {
        if IF_SPECTRUM_INVERTED {
            5e6 - d
        } else {
            5e6 + d
        }
    }

    fn convert(x: &[f64]) -> Vec<Complex32> {
        let mut out = Vec::new();
        let used = RealToIq::new().process_unpacked(&adc_bytes(x), &mut out);
        assert_eq!(used, x.len() * 2);
        out
    }

    /// The orientation under libairspy's assumptions: the R820T presents
    /// centre + d at the IF as 5 MHz - d. (This is the test the owner reads
    /// first if the live picture is mirrored.)
    #[test]
    fn libairspys_pattern_puts_an_if_below_5mhz_above_the_centre() {
        let n = 1 << 16;
        for d in [1e6, -1e6, 2.5e6, -3.5e6].map(on_bin) {
            let z = convert(&tone(rf_to_if(d), 0.8, n));
            let (f, _) = peak(&z);
            assert!(
                (f - d).abs() < 3e3,
                "RF offset {d} Hz came out at {f} Hz (IF_SPECTRUM_INVERTED = {IF_SPECTRUM_INVERTED})"
            );
            // And the pattern really is orientation-sensitive: the mirror
            // image of that IF (the other side of 5 MHz) comes out at -d.
            let z = convert(&tone(10e6 - rf_to_if(d), 0.8, n));
            let (f, _) = peak(&z);
            assert!(
                (f + d).abs() < 3e3,
                "mirror IF of {d} Hz came out at {f} Hz"
            );
        }
    }

    /// The same orientation derived independently: libairspy's pattern is the
    /// real signal times -exp(+j*pi*n/2), then a plain 31-tap complex FIR and
    /// decimation by 2. The fast path must agree with it sample for sample.
    #[test]
    fn matches_the_direct_complex_filter() {
        let n = 8192;
        // A mix of tones and noise-like content so every tap matters.
        let x: Vec<f64> = (0..n)
            .map(|k| {
                let t = k as f64;
                0.3 * (2.0 * PI * 4.1e6 * t / 20e6).cos()
                    + 0.2 * (2.0 * PI * 6.3e6 * t / 20e6 + 1.0).cos()
                    + 0.1 * (2.0 * PI * 3.0e6 * t / 20e6 + 2.0).cos()
                    + 0.05 * ((t * 12.9898).sin() * 43758.5453).fract()
            })
            .collect();
        let bytes = adc_bytes(&x);
        let mut fast = Vec::new();
        RealToIq::new().process_unpacked(&bytes, &mut fast);

        // What the ADC delivered, DC (the block mean) removed, normalised.
        let raw: Vec<f64> = (0..n)
            .map(|k| u16::from_le_bytes([bytes[2 * k], bytes[2 * k + 1]]) as f64)
            .collect();
        // The converter removes each block's mean; do the same per 4096.
        let mut cleaned = Vec::new();
        let mut dc = 0.0;
        for (b, blk) in raw.chunks(BLOCK).enumerate() {
            let mean = blk.iter().sum::<f64>() / blk.len() as f64;
            dc = if b == 0 {
                mean
            } else {
                dc + DC_ALPHA as f64 * (mean - dc)
            };
            cleaned.extend(blk.iter().map(|v| (v - dc) / 1024.0));
        }
        // Full 31-tap half-band, complex, at 20 MS/s.
        let mut h = [0.0f64; 31];
        for (i, &t) in HALF_BAND.iter().enumerate() {
            h[2 * i] = t as f64;
        }
        h[15] = 0.5;
        let y: Vec<Complex<f64>> = cleaned
            .iter()
            .enumerate()
            .map(|(k, &v)| {
                // -exp(+j*pi*k/2) = (-1, -j, +1, +j)
                let (pr, pi) = [(-1.0, 0.0), (0.0, -1.0), (1.0, 0.0), (0.0, 1.0)][k % 4];
                Complex::new(v * pr, v * pi)
            })
            .collect();
        // The fast path emits, for pair n, the filter centred on sample
        // 2(n - 8) + 1 (its latency is 8 pairs). Compare on that grid.
        let mut worst: f64 = 0.0;
        for (n_, f) in fast.iter().enumerate().skip(40) {
            let c = 2 * (n_ - 8) + 1;
            if c + 15 >= y.len() {
                break;
            }
            let mut acc = Complex::new(0.0, 0.0);
            for (t, &ht) in h.iter().enumerate() {
                if ht != 0.0 {
                    acc += y[c + t - 15] * ht;
                }
            }
            let e = (acc.re - f.re as f64)
                .abs()
                .max((Q_SIGN as f64 * acc.im - f.im as f64).abs());
            worst = worst.max(e);
        }
        assert!(
            worst < 1e-4,
            "fast path differs from the direct filter by {worst}"
        );
    }

    #[test]
    fn a_full_scale_tone_is_unity() {
        let z = convert(&tone(rf_to_if(on_bin(1e6)), 1.0, 1 << 16));
        let (f, m) = peak(&z);
        assert!((f - on_bin(1e6)).abs() < 3e3);
        assert!((m - 1.0).abs() < 0.02, "magnitude {m}");
        // I and Q each peak at about 1 (the clip logic reads either).
        let tail = &z[z.len() / 2..];
        let ipk = tail.iter().map(|c| c.re.abs()).fold(0.0, f32::max);
        let qpk = tail.iter().map(|c| c.im.abs()).fold(0.0, f32::max);
        assert!(
            (0.97..=1.03).contains(&ipk) && (0.97..=1.03).contains(&qpk),
            "{ipk} {qpk}"
        );
    }

    #[test]
    fn dc_offset_is_removed() {
        // Midpoint + 40 counts of DC, no signal.
        let bytes: Vec<u8> = (0..1 << 15).flat_map(|_| 2088u16.to_le_bytes()).collect();
        let mut out = Vec::new();
        RealToIq::new().process_unpacked(&bytes, &mut out);
        let p = out.iter().map(|c| c.norm_sqr()).sum::<f32>() / out.len() as f32;
        assert!(p < 1e-9, "residual power {p}");
    }

    #[test]
    fn block_boundaries_do_not_show() {
        // One call with the whole signal and the same signal in odd-sized
        // pieces (multiples of 8 bytes) agree.
        let x = tone(4.3e6, 0.5, 20_000);
        let bytes = adc_bytes(&x);
        let mut whole = Vec::new();
        RealToIq::new().process_unpacked(&bytes, &mut whole);
        let mut conv = RealToIq::new();
        let mut parts = Vec::new();
        for piece in bytes.chunks(8 * 300) {
            conv.process_unpacked(piece, &mut parts);
        }
        assert_eq!(whole.len(), parts.len());
        // The DC estimate follows a different block grid; the tone is far
        // from DC, so the difference is tiny.
        let worst = whole
            .iter()
            .zip(&parts)
            .skip(100)
            .map(|(a, b)| (a - b).norm())
            .fold(0.0, f32::max);
        assert!(worst < 2e-3, "{worst}");
    }

    #[test]
    fn half_band_response() {
        // 31 taps at 20 MS/s on a 1024-point FFT.
        let mut h = vec![Complex::new(0.0f64, 0.0); 1024];
        for (i, &t) in HALF_BAND.iter().enumerate() {
            h[2 * i] = Complex::new(t as f64, 0.0);
        }
        h[15] = Complex::new(0.5, 0.0);
        // The taps as designed sum to 0.99963 (-0.003 dB), not exactly 1.
        let sum: f64 = h.iter().map(|c| c.re).sum();
        assert!((sum - 1.0).abs() < 1e-3, "DC gain {sum}");
        for i in 0..8 {
            assert_eq!(HALF_BAND[i], HALF_BAND[15 - i], "tap {i} symmetric");
        }
        FftPlanner::new().plan_fft_forward(1024).process(&mut h);
        let db = |c: &Complex<f64>| 20.0 * c.norm().log10();
        let (mut ripple, mut stop) = (0.0f64, f64::MIN);
        for (k, c) in h.iter().enumerate().take(513) {
            let f = k as f64 * 20e6 / 1024.0;
            if f <= 3.9e6 {
                ripple = ripple.max(db(c).abs());
            }
            if f >= 6.1e6 {
                stop = stop.max(db(c));
            }
        }
        // A DVB-T 8 MHz channel is +-3.81 MHz about the centre.
        assert!(ripple < 0.02, "passband ripple {ripple} dB");
        assert!(stop < -58.0, "stopband {stop} dB");
    }

    /// Pack 12-bit samples the way libairspy's firmware does (the inverse of
    /// `unpack_12bit`).
    fn pack(s: &[u16]) -> Vec<u8> {
        let mut out = Vec::new();
        for o in s.as_chunks::<8>().0 {
            let o: Vec<u32> = o.iter().map(|&v| v as u32 & 0xfff).collect();
            let w0 = (o[0] << 20) | (o[1] << 8) | (o[2] >> 4);
            let w1 = (o[2] << 28) | (o[3] << 16) | (o[4] << 4) | (o[5] >> 8);
            let w2 = (o[5] << 24) | (o[6] << 12) | o[7];
            for w in [w0, w1, w2] {
                out.extend(w.to_le_bytes());
            }
        }
        out
    }

    #[test]
    fn unpacking_is_bit_exact() {
        // Every 12-bit value in every one of the 8 positions.
        for pos in 0..8 {
            for v in 0..4096u16 {
                let mut s = [0x555u16, 0xaaa, 0x123, 0xfff, 0x000, 0x800, 0x7ff, 0x3c3];
                s[pos] = v;
                let mut out = [0u16; 8];
                unpack_12bit(&pack(&s), &mut out);
                assert_eq!(out, s, "position {pos}, value {v:#x}");
            }
        }
        // A pseudo-random run.
        let mut x = 12345u32;
        let s: Vec<u16> = (0..8 * 500)
            .map(|_| {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                (x >> 20) as u16
            })
            .collect();
        let mut out = vec![0u16; s.len()];
        unpack_12bit(&pack(&s), &mut out);
        assert_eq!(out, s);
    }

    #[test]
    fn packed_and_unpacked_convert_alike() {
        let x = tone(3.2e6, 0.6, 8 * 5000);
        let raw: Vec<u16> = adc_bytes(&x)
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| u16::from_le_bytes(b))
            .collect();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        RealToIq::new().process_unpacked(&adc_bytes(&x), &mut a);
        let used = RealToIq::new().process_packed(&pack(&raw), &mut b);
        assert_eq!(used, raw.len() / 8 * 12);
        assert_eq!(a, b);
    }

    /// The radio's glitch: part of a packed transfer arrives as u16 words.
    #[test]
    fn unpacked_words_in_a_packed_transfer_are_blanked() {
        let x = tone(3.2e6, 0.6, 8 * 16384);
        let raw: Vec<u16> = adc_bytes(&x)
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| u16::from_le_bytes(b))
            .collect();
        let mut bytes = pack(&raw);
        // Clean packed data, a loud tone and a quiet input, never trips it.
        assert!(!bytes.chunks(6144).any(looks_unpacked));
        let quiet: Vec<u16> = (0..8 * 1024).map(|k| 2046 + (k % 5) as u16).collect();
        assert!(!looks_unpacked(&pack(&quiet)));
        // Bytes 512..12288 as the live glitch had them.
        let words: Vec<u8> = raw[..5888].iter().flat_map(|w| w.to_le_bytes()).collect();
        bytes[512..12288].copy_from_slice(&words);
        let mut conv = RealToIq::new();
        let mut out = Vec::new();
        assert_eq!(conv.process_packed(&bytes, &mut out), bytes.len());
        assert_eq!(out.len(), raw.len() / 2, "the sample count is kept");
        assert_eq!(conv.blanked(), 2, "the two 6144-byte blocks it touches");
        // The blanked stretch is near silence, not full-scale noise.
        let p = |s: &[Complex32]| s.iter().map(|c| c.norm_sqr()).sum::<f32>() / s.len() as f32;
        assert!(p(&out[100..4000]) < 0.01 * p(&out[8000..]));
    }

    #[test]
    fn a_short_transfer_uses_whole_groups_only() {
        let bytes = vec![0u8; 8 * 10 + 5];
        let mut out = Vec::new();
        assert_eq!(RealToIq::new().process_unpacked(&bytes, &mut out), 80);
        assert_eq!(out.len(), 20);
    }
}
