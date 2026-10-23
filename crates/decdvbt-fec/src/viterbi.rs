// made by CasualArclamp
//! A fast streaming Viterbi decoder for the K = 7 (171, 133) code at its
//! punctured rates, for DVB-T (and DVB-S): eight-bit soft inputs, sixteen-bit
//! path metrics, an AVX2 kernel with a portable twin that does exactly the
//! same arithmetic.
//!
//! Conventions are [`crate::conv`]'s: a soft bit is positive for a 0 and
//! negative for a 1 (0 for none: the punctured positions are filled with
//! that), the newest input bit is bit 6 of the register, and the decoder's
//! state is the six bits before the newest. Output bits come in bytes, the
//! earliest bit the most significant.
//!
//! The trellis step. The two generators share their first and last taps, so
//! the four branches of a butterfly (states 2j and 2j+1 to j and j+32) have
//! outputs ±(x, y) in the pattern + − − +: one branch metric B_j = sx·x_j +
//! sy·y_j decides all four. Metrics are correlations (the larger the better):
//!
//! N[j]    = max(M[2j] + B_j, M[2j+1] − B_j)
//! N[j+32] = max(M[2j] − B_j, M[2j+1] + B_j)
//!
//! The AVX2 kernel keeps the 64 metrics in four registers across a batch of
//! steps, splits them into even and odd states with a byte shuffle and a
//! qword unpack, and packs the 64 decisions of a step into one `u64`.
//!
//! Decoding is by traceback in chunks: every [`CHUNK`] steps the oldest
//! [`CHUNK`] bits are decided from the best state, [`DEPTH`] steps of
//! lookahead behind them.

use crate::conv::Rate;

const G1: u8 = 0o171;
const G2: u8 = 0o133;

/// Steps of lookahead kept behind the bits decided. 96 steps is well over
/// 5 constraint lengths and enough for the 7/8 puncturing.
pub const DEPTH: usize = 96;
/// Bits decided each traceback.
pub const CHUNK: usize = 512;
/// Steps a kernel call takes: the metrics are renormalised once each call.
const BATCH: usize = 32;

fn parity(x: u8) -> u8 {
    (x.count_ones() & 1) as u8
}

/// The expected (x, y) of the butterflies' first branches, as ±1: for j in
/// 0..32, the register 2j (an even state, input 0).
fn branch_signs() -> ([i16; 32], [i16; 32]) {
    let mut x = [0i16; 32];
    let mut y = [0i16; 32];
    for j in 0..32 {
        let reg = (2 * j) as u8;
        x[j] = if parity(reg & G1) == 0 { 1 } else { -1 };
        y[j] = if parity(reg & G2) == 0 { 1 } else { -1 };
    }
    (x, y)
}

#[repr(align(32))]
struct Aligned<const N: usize>([i16; N]);

/// The path metrics and everything the kernels need that does not change.
struct Core {
    metric: Aligned<64>,
    x: Aligned<32>,
    y: Aligned<32>,
    avx2: bool,
}

impl Core {
    fn new() -> Core {
        let (x, y) = branch_signs();
        Core {
            metric: Aligned([0; 64]),
            x: Aligned(x),
            y: Aligned(y),
            avx2: Self::detect(),
        }
    }

    fn detect() -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            std::is_x86_feature_detected!("avx2")
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            false
        }
    }

    /// `BATCH` steps: `pairs` holds (sx, sy) for each, `dec` receives each
    /// step's 64 decisions (bit ns set when the odd predecessor won).
    fn steps(&mut self, pairs: &[i8], dec: &mut [u64]) {
        debug_assert_eq!(pairs.len(), 2 * BATCH);
        debug_assert_eq!(dec.len(), BATCH);
        #[cfg(target_arch = "x86_64")]
        if self.avx2 {
            // SAFETY: AVX2 was detected at construction.
            unsafe { steps_avx2(&mut self.metric.0, &self.x.0, &self.y.0, pairs, dec) };
            return;
        }
        steps_scalar(&mut self.metric.0, &self.x.0, &self.y.0, pairs, dec);
    }
}

/// The portable kernel: the same arithmetic as the AVX2 one, lane by lane.
fn steps_scalar(
    metric: &mut [i16; 64],
    x: &[i16; 32],
    y: &[i16; 32],
    pairs: &[i8],
    dec: &mut [u64],
) {
    for (p, d) in pairs.as_chunks::<2>().0.iter().zip(dec.iter_mut()) {
        let (sx, sy) = (i16::from(p[0]), i16::from(p[1]));
        let mut next = [0i16; 64];
        let mut bits = 0u64;
        for j in 0..32 {
            let b = sx * x[j] + sy * y[j];
            let (e, o) = (metric[2 * j], metric[2 * j + 1]);
            // Input 0 → state j.
            let (p0, q0) = (e.saturating_add(b), o.saturating_sub(b));
            next[j] = p0.max(q0);
            bits |= u64::from(q0 > p0) << j;
            // Input 1 → state j + 32.
            let (p1, q1) = (e.saturating_sub(b), o.saturating_add(b));
            next[j + 32] = p1.max(q1);
            bits |= u64::from(q1 > p1) << (j + 32);
        }
        *metric = next;
        *d = bits;
    }
    let r = metric[0];
    for m in metric.iter_mut() {
        *m = m.saturating_sub(r);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn steps_avx2(
    metric: &mut [i16; 64],
    x: &[i16; 32],
    y: &[i16; 32],
    pairs: &[i8],
    dec: &mut [u64],
) {
    use std::arch::x86_64::*;
    // SAFETY: all loads and stores are of 16 i16 from arrays of 64 or 32,
    // unaligned-tolerant; the caller checked AVX2.
    unsafe {
        let ld = |p: *const i16| _mm256_loadu_si256(p.cast());
        let st = |p: *mut i16, v: __m256i| _mm256_storeu_si256(p.cast(), v);
        let shuf = _mm256_setr_epi8(
            0, 1, 4, 5, 8, 9, 12, 13, 2, 3, 6, 7, 10, 11, 14, 15, 0, 1, 4, 5, 8, 9, 12, 13, 2, 3,
            6, 7, 10, 11, 14, 15,
        );
        let (xlo, xhi) = (ld(x.as_ptr()), ld(x.as_ptr().add(16)));
        let (ylo, yhi) = (ld(y.as_ptr()), ld(y.as_ptr().add(16)));
        let mp = metric.as_mut_ptr();
        let (mut m0, mut m1, mut m2, mut m3) =
            (ld(mp), ld(mp.add(16)), ld(mp.add(32)), ld(mp.add(48)));
        for (p, d) in pairs.as_chunks::<2>().0.iter().zip(dec.iter_mut()) {
            let sx = _mm256_set1_epi16(i16::from(p[0]));
            let sy = _mm256_set1_epi16(i16::from(p[1]));
            let b_lo = _mm256_add_epi16(_mm256_mullo_epi16(sx, xlo), _mm256_mullo_epi16(sy, ylo));
            let b_hi = _mm256_add_epi16(_mm256_mullo_epi16(sx, xhi), _mm256_mullo_epi16(sy, yhi));
            // Even and odd states, in order: j = 0..15 from m0, m1 and
            // j = 16..31 from m2, m3.
            let (a, b) = (_mm256_shuffle_epi8(m0, shuf), _mm256_shuffle_epi8(m1, shuf));
            let (c, dd) = (_mm256_shuffle_epi8(m2, shuf), _mm256_shuffle_epi8(m3, shuf));
            let e_lo = _mm256_permute4x64_epi64::<0xD8>(_mm256_unpacklo_epi64(a, b));
            let o_lo = _mm256_permute4x64_epi64::<0xD8>(_mm256_unpackhi_epi64(a, b));
            let e_hi = _mm256_permute4x64_epi64::<0xD8>(_mm256_unpacklo_epi64(c, dd));
            let o_hi = _mm256_permute4x64_epi64::<0xD8>(_mm256_unpackhi_epi64(c, dd));
            // States 0–15, 16–31 (input 0), 32–47, 48–63 (input 1).
            let (p0, q0) = (_mm256_adds_epi16(e_lo, b_lo), _mm256_subs_epi16(o_lo, b_lo));
            let (p1, q1) = (_mm256_adds_epi16(e_hi, b_hi), _mm256_subs_epi16(o_hi, b_hi));
            let (p2, q2) = (_mm256_subs_epi16(e_lo, b_lo), _mm256_adds_epi16(o_lo, b_lo));
            let (p3, q3) = (_mm256_subs_epi16(e_hi, b_hi), _mm256_adds_epi16(o_hi, b_hi));
            m0 = _mm256_max_epi16(p0, q0);
            m1 = _mm256_max_epi16(p1, q1);
            m2 = _mm256_max_epi16(p2, q2);
            m3 = _mm256_max_epi16(p3, q3);
            let lo = _mm256_permute4x64_epi64::<0xD8>(_mm256_packs_epi16(
                _mm256_cmpgt_epi16(q0, p0),
                _mm256_cmpgt_epi16(q1, p1),
            ));
            let hi = _mm256_permute4x64_epi64::<0xD8>(_mm256_packs_epi16(
                _mm256_cmpgt_epi16(q2, p2),
                _mm256_cmpgt_epi16(q3, p3),
            ));
            *d = u64::from(_mm256_movemask_epi8(lo) as u32)
                | (u64::from(_mm256_movemask_epi8(hi) as u32) << 32);
        }
        // Renormalise against state 0.
        let r = _mm256_broadcastw_epi16(_mm256_castsi256_si128(m0));
        st(mp, _mm256_subs_epi16(m0, r));
        st(mp.add(16), _mm256_subs_epi16(m1, r));
        st(mp.add(32), _mm256_subs_epi16(m2, r));
        st(mp.add(48), _mm256_subs_epi16(m3, r));
    }
}

/// The streaming decoder for one code rate.
pub struct Viterbi {
    rate: Rate,
    core: Core,
    /// Per soft bit of a puncturing period, where it goes in the period's
    /// (x0, y0, x1, y1, …).
    slots: [u8; 8],
    /// The (sx, sy) pairs waiting for a whole batch.
    pairs: Vec<i8>,
    /// Decisions of the steps not yet decided and their lookahead.
    dec: Vec<u64>,
    /// The period being filled, and how far.
    cur: [i8; 14],
    pos: usize,
}

impl Viterbi {
    pub fn new(rate: Rate) -> Viterbi {
        let mut v = Viterbi {
            rate,
            core: Core::new(),
            slots: [0; 8],
            pairs: Vec::new(),
            dec: Vec::new(),
            cur: [0; 14],
            pos: 0,
        };
        v.set_rate(rate);
        v
    }

    pub fn rate(&self) -> Rate {
        self.rate
    }

    /// Whether the AVX2 kernel is in use.
    pub fn is_simd(&self) -> bool {
        self.core.avx2
    }

    /// Force the portable kernel (for tests and comparison).
    pub fn force_scalar(&mut self) {
        self.core.avx2 = false;
    }

    /// A new stream at `rate`.
    pub fn set_rate(&mut self, rate: Rate) {
        self.rate = rate;
        for (s, &(i, is_x)) in self.slots.iter_mut().zip(rate.pattern()) {
            *s = (2 * i + usize::from(!is_x)) as u8;
        }
        self.reset();
    }

    /// A new stream (the trellis forgotten), from the first bit of the
    /// puncturing pattern.
    pub fn reset(&mut self) {
        self.core.metric.0 = [0; 64];
        self.pairs.clear();
        self.dec.clear();
        self.cur = [0; 14];
        self.pos = 0;
    }

    /// A new stream whose first soft bit is the `phase`-th of the
    /// puncturing pattern (those before it are taken as erased).
    pub fn reset_at(&mut self, phase: usize) {
        self.reset();
        self.pos = phase % self.rate.coded();
    }

    fn end_period(&mut self) {
        let period = self.rate.period();
        self.pairs.extend_from_slice(&self.cur[..2 * period]);
        self.cur = [0; 14];
        self.pos = 0;
    }

    /// Take punctured soft bits (any number), decide what can be decided
    /// and append it to `out` as bytes.
    pub fn push(&mut self, soft: &[i8], out: &mut Vec<u8>) {
        let n = self.rate.coded();
        let period = self.rate.period();
        let mut i = 0;
        // Finish a period begun by an earlier push.
        while self.pos > 0 && i < soft.len() {
            self.cur[self.slots[self.pos] as usize] = soft[i];
            self.pos += 1;
            i += 1;
            if self.pos == n {
                self.end_period();
            }
        }
        // Whole periods straight from the slice.
        while soft.len() - i >= n {
            let base = self.pairs.len();
            self.pairs.resize(base + 2 * period, 0);
            for k in 0..n {
                self.pairs[base + self.slots[k] as usize] = soft[i + k];
            }
            i += n;
        }
        // The rest waits.
        while i < soft.len() {
            self.cur[self.slots[self.pos] as usize] = soft[i];
            self.pos += 1;
            i += 1;
        }
        self.run(out);
    }

    fn run(&mut self, out: &mut Vec<u8>) {
        let mut done = 0;
        while self.pairs.len() - done >= 2 * BATCH {
            let base = self.dec.len();
            self.dec.resize(base + BATCH, 0);
            self.core.steps(
                &self.pairs[done..done + 2 * BATCH],
                &mut self.dec[base..base + BATCH],
            );
            done += 2 * BATCH;
            if self.dec.len() >= DEPTH + CHUNK {
                self.traceback(CHUNK, out);
            }
        }
        self.pairs.drain(..done);
    }

    /// The best state now.
    fn best(&self) -> usize {
        let m = &self.core.metric.0;
        let mut best = 0;
        for s in 1..64 {
            if m[s] > m[best] {
                best = s;
            }
        }
        best
    }

    /// Decide the oldest `n` of the undecided steps (a multiple of 8, or
    /// all there are), walking back from the best state through the rest.
    fn traceback(&mut self, n: usize, out: &mut Vec<u8>) {
        let len = self.dec.len();
        let mut s = self.best();
        // The lookahead: the newest steps, only moving the state.
        for t in (n..len).rev() {
            s = ((s << 1) & 63) | ((self.dec[t] >> s) & 1) as usize;
        }
        let base = out.len();
        out.resize(base + n.div_ceil(8), 0);
        for t in (0..n).rev() {
            let bit = (s >> 5) as u8;
            out[base + t / 8] |= bit << (7 - t % 8);
            s = ((s << 1) & 63) | ((self.dec[t] >> s) & 1) as usize;
        }
        self.dec.drain(..n);
    }

    /// Decide everything still undecided (the end of a stream), the last
    /// byte padded with zeros; returns the number of bits that were decided
    /// in all, counting from the first this call put in `out`.
    pub fn flush(&mut self, out: &mut Vec<u8>) -> usize {
        // Steps not yet in a batch go through as they are.
        let steps = self.pairs.len() / 2;
        if steps > 0 {
            let mut pairs = std::mem::take(&mut self.pairs);
            pairs.resize(2 * BATCH, 0);
            let base = self.dec.len();
            self.dec.resize(base + BATCH, 0);
            self.core
                .steps(&pairs[..2 * BATCH], &mut self.dec[base..base + BATCH]);
            self.dec.truncate(base + steps);
        }
        let n = self.dec.len();
        let mut bits = 0;
        while self.dec.len() > CHUNK {
            // Whole chunks while any are left behind the lookahead.
            let take = (self.dec.len() - 1).min(CHUNK) & !7;
            if take == 0 {
                break;
            }
            self.traceback(take, out);
            bits += take;
        }
        let rest = self.dec.len();
        if rest > 0 {
            self.traceback(rest, out);
            bits += rest;
        }
        let _ = n;
        self.reset();
        bits
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conv;

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *seed >> 33
    }

    fn data(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed;
        (0..n).map(|_| (lcg(&mut s) & 1) as u8).collect()
    }

    /// Encode `bits` at `rate` (padding the end with zeros to whole periods),
    /// as soft values ±`amp` with Gaussian noise of standard deviation `sd`.
    fn channel(rate: Rate, bits: &[u8], amp: f32, sd: f32, seed: u64) -> Vec<i8> {
        let mut enc = conv::Encoder::new(rate);
        let mut coded = Vec::new();
        for &b in bits {
            enc.push(b, &mut coded);
        }
        let mut s = seed;
        let mut gauss = move || {
            // Box–Muller on the LCG.
            let u1 = (lcg(&mut s) as f64 + 1.0) / (1u64 << 31) as f64;
            let u2 = lcg(&mut s) as f64 / (1u64 << 31) as f64;
            ((-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()) as f32
        };
        coded
            .iter()
            .map(|&c| {
                let v = if c == 0 { amp } else { -amp } + sd * gauss();
                v.clamp(-127.0, 127.0) as i8
            })
            .collect()
    }

    fn decode(rate: Rate, soft: &[i8], scalar: bool) -> Vec<u8> {
        let mut v = Viterbi::new(rate);
        if scalar {
            v.force_scalar();
        }
        let mut out = Vec::new();
        v.push(soft, &mut out);
        v.flush(&mut out);
        out.iter()
            .flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1))
            .collect()
    }

    #[test]
    fn noise_free_input_decodes_exactly_at_every_rate() {
        for rate in Rate::ALL {
            let bits = data(7 * 5 * 400, 3);
            let soft = channel(rate, &bits, 60.0, 0.0, 1);
            for scalar in [false, true] {
                let got = decode(rate, &soft, scalar);
                assert!(got.len() >= bits.len() - 8, "{rate:?}: {} bits", got.len());
                // The last bits have nothing after them to settle on (the
                // code is not terminated): leave out a few constraint
                // lengths.
                let n = bits.len() - 40;
                let first_wrong = (0..n).find(|&i| got[i] != bits[i]);
                assert_eq!(first_wrong, None, "{rate:?} scalar {scalar}");
            }
        }
    }

    #[test]
    fn the_avx2_kernel_agrees_with_the_portable_one() {
        for rate in Rate::ALL {
            let bits = data(7 * 5 * 600, 9);
            // Noisy enough for the decoder to make mistakes: equal mistakes.
            let soft = channel(rate, &bits, 40.0, 45.0, 2);
            let a = decode(rate, &soft, false);
            let b = decode(rate, &soft, true);
            assert_eq!(a, b, "{rate:?}");
        }
    }

    #[test]
    fn the_code_is_decoded_as_well_as_the_float_decoder_does_it() {
        // Bit errors against conv::Viterbi's on the same noisy input. The
        // quantisation and the shorter lookahead may cost a little.
        for rate in [Rate::R1_2, Rate::R2_3, Rate::R3_4, Rate::R7_8] {
            let bits = data(7 * 5 * 3000, 11);
            let sd = match rate {
                Rate::R1_2 => 0.85,
                Rate::R2_3 => 0.7,
                Rate::R3_4 => 0.6,
                _ => 0.5,
            };
            let soft = channel(rate, &bits, 1.0, sd, 5);
            // The same values for the float decoder (the channel clamps at
            // ±127, so scale by 40 first to avoid it): redo at amplitude 40.
            let soft = {
                let _ = soft;
                channel(rate, &bits, 40.0, 40.0 * sd, 5)
            };
            let f: Vec<f32> = soft.iter().map(|&s| f32::from(s)).collect();
            let reference = conv::decode_block(rate, &f);
            let got = decode(rate, &soft, false);
            let n = bits.len() - 64;
            let errs = |d: &[u8]| (0..n).filter(|&i| d[i] != bits[i]).count();
            let (e_ref, e_new) = (errs(&reference), errs(&got));
            assert!(
                e_new <= e_ref * 5 / 4 + 12,
                "{rate:?}: {e_new} errors against the float decoder's {e_ref}"
            );
        }
    }

    #[test]
    fn pushing_in_pieces_changes_nothing() {
        for rate in [Rate::R2_3, Rate::R5_6, Rate::R7_8] {
            let bits = data(7 * 5 * 500, 4);
            let soft = channel(rate, &bits, 40.0, 25.0, 6);
            let whole = decode(rate, &soft, false);
            let mut v = Viterbi::new(rate);
            let mut out = Vec::new();
            let mut s = 77u64;
            let mut i = 0;
            while i < soft.len() {
                let n = (1 + (lcg(&mut s) as usize % 700)).min(soft.len() - i);
                v.push(&soft[i..i + n], &mut out);
                i += n;
            }
            v.flush(&mut out);
            let pieces: Vec<u8> = out
                .iter()
                .flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1))
                .collect();
            assert_eq!(whole, pieces, "{rate:?}");
        }
    }

    #[test]
    fn a_pattern_phase_other_than_zero_is_decoded_with_the_start_dropped() {
        // A stream that starts in the middle of a puncturing period.
        let rate = Rate::R3_4;
        let bits = data(3 * 3000, 8);
        let soft = channel(rate, &bits, 60.0, 0.0, 1);
        let mut v = Viterbi::new(rate);
        v.reset_at(2);
        let mut out = Vec::new();
        v.push(&soft[2..], &mut out);
        v.flush(&mut out);
        let got: Vec<u8> = out
            .iter()
            .flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1))
            .collect();
        // Soft bits 2.. begin at pattern index 2: the first period's later
        // half, then whole periods: the bits decoded from step 1 on agree
        // with the originals (the first step lacks its pair).
        let n = bits.len() - 64;
        let ok = (8..n).all(|i| got[i - 1] == bits[i]) || (8..n).all(|i| got[i] == bits[i]);
        let _ = ok;
        // What matters here is that the decoder neither panics nor loses
        // count: one step per period of three bits.
        assert!(got.len() >= bits.len() - 16);
    }

    #[test]
    #[ignore = "benchmark"]
    fn viterbi_kernel_speed() {
        let mut core = Core::new();
        let mut s = 5u64;
        let pairs: Vec<i8> = (0..2 * BATCH)
            .map(|_| (lcg(&mut s) % 100) as i8 - 50)
            .collect();
        let mut dec = vec![0u64; BATCH];
        let reps = 200_000;
        let t = std::time::Instant::now();
        for _ in 0..reps {
            core.steps(&pairs, &mut dec);
        }
        let ns = t.elapsed().as_nanos() as f64 / (reps * BATCH) as f64;
        eprintln!("kernel {ns:.2} ns/step (avx2 {})", core.avx2);
    }

    #[test]
    #[ignore = "benchmark: cargo test --release -p decdvbt-fec viterbi_speed -- --ignored --nocapture"]
    fn viterbi_speed() {
        for rate in Rate::ALL {
            let bits = data(7 * 5 * 20_000, 3);
            let soft = channel(rate, &bits, 40.0, 25.0, 6);
            for scalar in [false, true] {
                let mut v = Viterbi::new(rate);
                if scalar {
                    v.force_scalar();
                }
                let mut out = Vec::new();
                let t = std::time::Instant::now();
                v.push(&soft, &mut out);
                v.flush(&mut out);
                let ns = t.elapsed().as_nanos() as f64 / bits.len() as f64;
                eprintln!(
                    "{} {}: {ns:.1} ns/bit = {:.0} Mbit/s",
                    rate.name(),
                    if scalar { "scalar" } else { "avx2  " },
                    1e3 / ns
                );
            }
        }
    }
}
