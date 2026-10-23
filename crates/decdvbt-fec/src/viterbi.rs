// made by CasualArclamp
//! A fast streaming Viterbi decoder for the K = 7 (171, 133) code at its
//! punctured rates, for DVB-T (and DVB-S): soft inputs of eight bits (±127),
//! sixteen-bit path metrics, an AVX2 kernel and a portable twin that does
//! exactly the same arithmetic.
//!
//! (An eight-bit-metric variant was built and measured: 4.7 against 8.3 ns a
//! bit, but with soft bits weighted by the channel's gain and noise per carrier
//! the useful range is far wider than ±4, and at the best fixed scale it lost
//! 0.3–0.4 dB: 1.5× the error rate into the Reed–Solomon code on a marginal
//! mux. Not kept.)
//!
//! Conventions are [`crate::conv`]'s: a soft bit is positive for a 0 and
//! negative for a 1 (0 for none: the punctured positions are filled with
//! that), the newest input bit is bit 6 of the register, and the decoder's
//! state is the six bits before the newest. Output bits come in bytes, the
//! earliest bit the most significant.
//!
//! **The trellis step.** The two generators share their first and last taps, so
//! the four branches of a butterfly (states 2j and 2j+1 to j and j+32) have
//! outputs ±(x, y) in the pattern + − − +: one branch metric B_j = sx·x_j +
//! sy·y_j decides all four. Metrics are correlations (the larger the better):
//!
//! N[j]    = max(M[2j] + B_j, M[2j+1] − B_j)
//! N[j+32] = max(M[2j] − B_j, M[2j+1] + B_j)
//!
//! A kernel keeps the 64 metrics in registers, splits them into even and odd
//! states with a byte shuffle and a qword unpack, and puts the 64 decisions of
//! a step in one `u64`.
//!
//! **Two trellises at once.** One trellis is a chain of dependent steps: each
//! waits for the shuffle, the add and the compare of the one before, and the
//! kernel spends its time waiting, not working. So the stream is cut into
//! *segments* that are decoded in pairs, two independent chains interleaved in
//! one loop. Each segment starts [`ACQ`] steps ahead of the bits it decides
//! (from an unknown state, all metrics equal: the first steps settle it) and
//! ends [`DEPTH`] steps after them (the lookahead a traceback needs), so the
//! segments overlap and the bits come out as if there had been no cut; the cost
//! is (ACQ + DEPTH)/W more steps, 5 % at W = [`SEGMENT`]. The output lags the
//! input by up to two segments. Depuncturing is a byte shuffle: a period of the
//! puncturing pattern is a fixed gather of coded soft bits into (x, y) slots
//! with zeros for the erased ones, done 8–16 bytes at a time.

use crate::conv::Rate;

const G1: u8 = 0o171;
const G2: u8 = 0o133;

/// Steps of lookahead after the bits decided: well over 5 constraint lengths
/// and enough for the 7/8 puncturing.
pub const DEPTH: usize = 96;
/// Steps a segment runs from an unknown state before the bits it decides.
pub const ACQ: usize = 96;
/// Bits decided by each segment.
pub const SEGMENT: usize = 4096;
/// Steps in a segment run, a multiple of 32 (the precise kernel renormalises
/// its metrics every 32).
const SEG_STEPS: usize = ACQ + SEGMENT + DEPTH;

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
struct Aligned<T, const N: usize>([T; N]);

/// The tables the kernels need, and whether to use the AVX2 one.
struct Core {
    x: Aligned<i16, 32>,
    y: Aligned<i16, 32>,
    avx2: bool,
}

/// The best state of a final set of metrics (the largest).
fn best16(m: &[i16; 64]) -> usize {
    let mut best = 0;
    for s in 1..64 {
        if m[s] > m[best] {
            best = s;
        }
    }
    best
}

impl Core {
    fn new() -> Core {
        let (x, y) = branch_signs();
        Core {
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

    /// Run two segments of `SEG_STEPS` steps each from all-equal metrics:
    /// `pairs_*` hold the (sx, sy) of each step, `dec_*` receive each step's 64
    /// decisions (bit ns set when the odd predecessor won). Returns the two
    /// segments' best states at the end.
    fn run(
        &self,
        pairs_a: &[i8],
        pairs_b: &[i8],
        dec_a: &mut [u64],
        dec_b: &mut [u64],
    ) -> (usize, usize) {
        debug_assert!(pairs_a.len() == 2 * SEG_STEPS && pairs_b.len() == 2 * SEG_STEPS);
        debug_assert!(dec_a.len() == SEG_STEPS && dec_b.len() == SEG_STEPS);
        #[cfg(target_arch = "x86_64")]
        if self.avx2 {
            // SAFETY: AVX2 was detected at construction.
            return unsafe { run16_avx2(&self.x.0, &self.y.0, pairs_a, pairs_b, dec_a, dec_b) };
        }
        (
            seg16_scalar(&self.x.0, &self.y.0, pairs_a, dec_a),
            seg16_scalar(&self.x.0, &self.y.0, pairs_b, dec_b),
        )
    }
}

/// One segment, portable, precise: the same arithmetic as the AVX2 kernel,
/// lane by lane, renormalising against state 0 every 32 steps.
fn seg16_scalar(x: &[i16; 32], y: &[i16; 32], pairs: &[i8], dec: &mut [u64]) -> usize {
    let mut metric = [0i16; 64];
    for (n, (p, d)) in pairs
        .as_chunks::<2>()
        .0
        .iter()
        .zip(dec.iter_mut())
        .enumerate()
    {
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
        metric = next;
        *d = bits;
        if n % 32 == 31 {
            let r = metric[0];
            for m in &mut metric {
                *m = m.saturating_sub(r);
            }
        }
    }
    best16(&metric)
}

/// One step of the precise kernel on a set of four metric registers.
#[cfg(target_arch = "x86_64")]
macro_rules! step16 {
    ($m0:ident, $m1:ident, $m2:ident, $m3:ident, $p:expr, $c:ident) => {{
        use std::arch::x86_64::*;
        let sx = _mm256_set1_epi16(i16::from($p[0]));
        let sy = _mm256_set1_epi16(i16::from($p[1]));
        let b_lo = _mm256_add_epi16(_mm256_sign_epi16(sx, $c.xlo), _mm256_sign_epi16(sy, $c.ylo));
        let b_hi = _mm256_add_epi16(_mm256_sign_epi16(sx, $c.xhi), _mm256_sign_epi16(sy, $c.yhi));
        // Even and odd states, in order: j = 0..15 from m0, m1 and
        // j = 16..31 from m2, m3.
        let (a, b) = (
            _mm256_shuffle_epi8($m0, $c.shuf),
            _mm256_shuffle_epi8($m1, $c.shuf),
        );
        let (c2, dd) = (
            _mm256_shuffle_epi8($m2, $c.shuf),
            _mm256_shuffle_epi8($m3, $c.shuf),
        );
        let e_lo = _mm256_permute4x64_epi64::<0xD8>(_mm256_unpacklo_epi64(a, b));
        let o_lo = _mm256_permute4x64_epi64::<0xD8>(_mm256_unpackhi_epi64(a, b));
        let e_hi = _mm256_permute4x64_epi64::<0xD8>(_mm256_unpacklo_epi64(c2, dd));
        let o_hi = _mm256_permute4x64_epi64::<0xD8>(_mm256_unpackhi_epi64(c2, dd));
        // States 0–15, 16–31 (input 0), 32–47, 48–63 (input 1).
        let (p0, q0) = (_mm256_adds_epi16(e_lo, b_lo), _mm256_subs_epi16(o_lo, b_lo));
        let (p1, q1) = (_mm256_adds_epi16(e_hi, b_hi), _mm256_subs_epi16(o_hi, b_hi));
        let (p2, q2) = (_mm256_subs_epi16(e_lo, b_lo), _mm256_adds_epi16(o_lo, b_lo));
        let (p3, q3) = (_mm256_subs_epi16(e_hi, b_hi), _mm256_adds_epi16(o_hi, b_hi));
        $m0 = _mm256_max_epi16(p0, q0);
        $m1 = _mm256_max_epi16(p1, q1);
        $m2 = _mm256_max_epi16(p2, q2);
        $m3 = _mm256_max_epi16(p3, q3);
        let lo = _mm256_permute4x64_epi64::<0xD8>(_mm256_packs_epi16(
            _mm256_cmpgt_epi16(q0, p0),
            _mm256_cmpgt_epi16(q1, p1),
        ));
        let hi = _mm256_permute4x64_epi64::<0xD8>(_mm256_packs_epi16(
            _mm256_cmpgt_epi16(q2, p2),
            _mm256_cmpgt_epi16(q3, p3),
        ));
        u64::from(_mm256_movemask_epi8(lo) as u32)
            | (u64::from(_mm256_movemask_epi8(hi) as u32) << 32)
    }};
}

/// The precise kernel's constants, in registers.
#[cfg(target_arch = "x86_64")]
struct Consts16 {
    xlo: std::arch::x86_64::__m256i,
    xhi: std::arch::x86_64::__m256i,
    ylo: std::arch::x86_64::__m256i,
    yhi: std::arch::x86_64::__m256i,
    shuf: std::arch::x86_64::__m256i,
}

/// Renormalise four metric registers against state 0 (the first lane of the
/// first).
#[cfg(target_arch = "x86_64")]
macro_rules! renorm16 {
    ($m0:ident, $m1:ident, $m2:ident, $m3:ident) => {{
        use std::arch::x86_64::*;
        let r = _mm256_broadcastw_epi16(_mm256_castsi256_si128($m0));
        $m0 = _mm256_subs_epi16($m0, r);
        $m1 = _mm256_subs_epi16($m1, r);
        $m2 = _mm256_subs_epi16($m2, r);
        $m3 = _mm256_subs_epi16($m3, r);
    }};
}

/// Two segments, precise, interleaved.
///
/// # Safety
/// AVX2 must be present.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn run16_avx2(
    x: &[i16; 32],
    y: &[i16; 32],
    pairs_a: &[i8],
    pairs_b: &[i8],
    dec_a: &mut [u64],
    dec_b: &mut [u64],
) -> (usize, usize) {
    use std::arch::x86_64::*;
    // SAFETY: loads are of 16 i16 from arrays of 32, stores to local arrays
    // of 64; the caller checked AVX2.
    unsafe {
        let ld = |p: *const i16| _mm256_loadu_si256(p.cast());
        let c = Consts16 {
            xlo: ld(x.as_ptr()),
            xhi: ld(x.as_ptr().add(16)),
            ylo: ld(y.as_ptr()),
            yhi: ld(y.as_ptr().add(16)),
            shuf: _mm256_setr_epi8(
                0, 1, 4, 5, 8, 9, 12, 13, 2, 3, 6, 7, 10, 11, 14, 15, 0, 1, 4, 5, 8, 9, 12, 13, 2,
                3, 6, 7, 10, 11, 14, 15,
            ),
        };
        let z = _mm256_setzero_si256();
        let (mut a0, mut a1, mut a2, mut a3) = (z, z, z, z);
        let (mut b0, mut b1, mut b2, mut b3) = (z, z, z, z);
        // Blocks of 32 steps, the metrics renormalised after each.
        let blocks = pairs_a
            .as_chunks::<64>()
            .0
            .iter()
            .zip(pairs_b.as_chunks::<64>().0)
            .zip(
                dec_a
                    .as_chunks_mut::<32>()
                    .0
                    .iter_mut()
                    .zip(dec_b.as_chunks_mut::<32>().0.iter_mut()),
            );
        for ((pa, pb), (da, db)) in blocks {
            let steps = pa
                .as_chunks::<2>()
                .0
                .iter()
                .zip(pb.as_chunks::<2>().0)
                .zip(da.iter_mut().zip(db.iter_mut()));
            for ((p, q), (d, e)) in steps {
                *d = step16!(a0, a1, a2, a3, p, c);
                *e = step16!(b0, b1, b2, b3, q, c);
            }
            renorm16!(a0, a1, a2, a3);
            renorm16!(b0, b1, b2, b3);
        }
        let (mut ma, mut mb) = ([0i16; 64], [0i16; 64]);
        for (i, v) in [a0, a1, a2, a3].into_iter().enumerate() {
            _mm256_storeu_si256(ma.as_mut_ptr().add(16 * i).cast(), v);
        }
        for (i, v) in [b0, b1, b2, b3].into_iter().enumerate() {
            _mm256_storeu_si256(mb.as_mut_ptr().add(16 * i).cast(), v);
        }
        (best16(&ma), best16(&mb))
    }
}

/// The depuncturing shuffle over `blocks` blocks: each loads 16 bytes of coded
/// soft bits at `in_step` intervals, gathers them by `mask` (0x80: a zero, an
/// erasure) and stores 16 bytes at `out_step` intervals, clipped to ±`limit`.
///
/// # Safety
/// AVX2 (SSSE3 and SSE4.1) must be present; `soft` must hold 16 bytes from
/// each block's start and `pairs` 16 from each block's output.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn gather_blocks(
    soft: &[i8],
    pairs: &mut [i8],
    blocks: usize,
    mask: &[u8; 16],
    in_step: usize,
    out_step: usize,
    limit: i8,
) {
    use std::arch::x86_64::*;
    // SAFETY: the contract above.
    unsafe {
        let m = _mm_loadu_si128(mask.as_ptr().cast());
        let hi = _mm_set1_epi8(limit);
        let lo = _mm_set1_epi8(-limit);
        let (src, dst) = (soft.as_ptr(), pairs.as_mut_ptr());
        for b in 0..blocks {
            let v = _mm_loadu_si128(src.add(b * in_step).cast());
            let v = _mm_max_epi8(_mm_min_epi8(_mm_shuffle_epi8(v, m), hi), lo);
            _mm_storeu_si128(dst.add(b * out_step).cast(), v);
        }
    }
}

/// The state one step back: its decision bit is the oldest bit.
#[inline(always)]
fn back(s: usize, d: u64) -> usize {
    ((s << 1) & 63) | ((d >> s) & 1) as usize
}

/// The streaming decoder for one code rate.
pub struct Viterbi {
    rate: Rate,
    core: Core,
    /// Per soft bit of a puncturing period, where it goes in the period's
    /// (x0, y0, x1, y1, …).
    slots: [u8; 8],
    /// The (sx, sy) pairs of the steps since [`ACQ`] before the first not yet
    /// decided; `ctx` is how many of the steps at the front are that context.
    pairs: Vec<i8>,
    ctx: usize,
    /// The decisions of the two segments being decoded.
    dec_a: Vec<u64>,
    dec_b: Vec<u64>,
    /// The period being filled, and how far.
    cur: [i8; 14],
    pos: usize,
    /// The depuncturing shuffle for a block of whole periods: the mask, the
    /// coded soft bits it takes and the (x, y) bytes it makes.
    gather: [u8; 16],
    gather_in: usize,
    gather_out: usize,
}

impl Viterbi {
    pub fn new(rate: Rate) -> Viterbi {
        let mut v = Viterbi {
            rate,
            core: Core::new(),
            slots: [0; 8],
            pairs: Vec::new(),
            ctx: 0,
            dec_a: vec![0; SEG_STEPS],
            dec_b: vec![0; SEG_STEPS],
            cur: [0; 14],
            pos: 0,
            gather: [0x80; 16],
            gather_in: 0,
            gather_out: 0,
        };
        v.set_rate(rate);
        v
    }

    pub fn rate(&self) -> Rate {
        self.rate
    }

    /// Whether the AVX2 kernels are in use.
    pub fn is_simd(&self) -> bool {
        self.core.avx2
    }

    /// Force the portable kernels (for tests and comparison).
    pub fn force_scalar(&mut self) {
        self.core.avx2 = false;
    }

    /// A new stream at `rate`.
    pub fn set_rate(&mut self, rate: Rate) {
        self.rate = rate;
        for (s, &(i, is_x)) in self.slots.iter_mut().zip(rate.pattern()) {
            *s = (2 * i + usize::from(!is_x)) as u8;
        }
        // As many whole periods as one 16-byte shuffle takes in and puts out.
        let (n, p2) = (rate.coded(), 2 * rate.period());
        let k = (16 / n).min(16 / p2);
        self.gather = [0x80; 16];
        for c in 0..k * n {
            self.gather[(c / n) * p2 + self.slots[c % n] as usize] = c as u8;
        }
        self.gather_in = k * n;
        self.gather_out = k * p2;
        self.reset();
    }

    /// A new stream (the trellis forgotten), from the first bit of the
    /// puncturing pattern.
    pub fn reset(&mut self) {
        self.pairs.clear();
        self.ctx = 0;
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
        self.fill(soft);
        self.run(out);
    }

    /// Depuncture `soft` onto the waiting (sx, sy) pairs.
    fn fill(&mut self, soft: &[i8]) {
        let limit = i8::MAX;
        let n = self.rate.coded();
        let period = self.rate.period();
        let mut i = 0;
        // Finish a period begun by an earlier push.
        while self.pos > 0 && i < soft.len() {
            self.cur[self.slots[self.pos] as usize] = soft[i].clamp(-limit, limit);
            self.pos += 1;
            i += 1;
            if self.pos == n {
                self.end_period();
            }
        }
        // Whole periods straight from the slice, by shuffle where the slice is
        // long enough for a 16-byte load.
        #[cfg(target_arch = "x86_64")]
        if self.core.avx2 && soft.len() - i >= 16 {
            let (g_in, g_out) = (self.gather_in, self.gather_out);
            let blocks = (soft.len() - i - 16) / g_in + 1;
            let base = self.pairs.len();
            // 16 bytes are stored for each block, `g_out` kept.
            self.pairs.resize(base + blocks * g_out + 16, 0);
            // SAFETY: AVX2 (so SSSE3 and SSE4.1) was detected; the loads stay
            // inside `soft` (16 bytes from a block's start, which `blocks`
            // keeps in) and the stores inside `pairs` (16 bytes from
            // `base + b·g_out`, room for the last by the `+ 16`).
            unsafe {
                gather_blocks(
                    &soft[i..],
                    &mut self.pairs[base..],
                    blocks,
                    &self.gather,
                    g_in,
                    g_out,
                    limit,
                );
            }
            self.pairs.truncate(base + blocks * g_out);
            i += blocks * g_in;
        }
        while soft.len() - i >= n {
            let base = self.pairs.len();
            self.pairs.resize(base + 2 * period, 0);
            for k in 0..n {
                self.pairs[base + self.slots[k] as usize] = soft[i + k].clamp(-limit, limit);
            }
            i += n;
        }
        // The rest waits.
        while i < soft.len() {
            self.cur[self.slots[self.pos] as usize] = soft[i].clamp(-limit, limit);
            self.pos += 1;
            i += 1;
        }
    }

    /// Steps waiting to be decided (the context before them not counted).
    fn pending(&self) -> usize {
        self.pairs.len() / 2 - self.ctx
    }

    /// Decode while two whole segments (and the lookahead after them) are
    /// waiting.
    fn run(&mut self, out: &mut Vec<u8>) {
        while self.pending() >= 2 * SEGMENT + DEPTH {
            self.super_block(out, 2 * SEGMENT);
        }
    }

    /// Decode two segments from the front of the pending steps, appending
    /// `keep` of their 2·[`SEGMENT`] bits to `out` (all, but at the end of a
    /// stream).
    fn super_block(&mut self, out: &mut Vec<u8>, keep: usize) {
        let ctx = self.ctx;
        // Segment A: the context (zeros, erasures, if the stream is new), its
        // bits and their lookahead. Segment B: ACQ steps before its bits.
        let a_start = if ctx >= ACQ { 0 } else { 2 * (ACQ - ctx) };
        let mut padded = Vec::new();
        let pa: &[i8] = if ctx >= ACQ {
            &self.pairs[..2 * SEG_STEPS]
        } else {
            padded.resize(a_start, 0);
            padded.extend_from_slice(&self.pairs[..2 * (SEG_STEPS - (ACQ - ctx))]);
            &padded
        };
        let b0 = ctx + SEGMENT - ACQ;
        let pb = &self.pairs[2 * b0..2 * (b0 + SEG_STEPS)];
        let (best_a, best_b) = self.core.run(pa, pb, &mut self.dec_a, &mut self.dec_b);

        let base = out.len();
        out.resize(base + 2 * SEGMENT / 8, 0);
        let (oa, ob) = out[base..].split_at_mut(SEGMENT / 8);
        traceback2([&self.dec_a, &self.dec_b], [best_a, best_b], [oa, ob]);
        out.truncate(base + keep.div_ceil(8));

        // The next segment's context: ACQ steps before what is still to come.
        self.pairs.drain(..2 * (ctx + 2 * SEGMENT - ACQ));
        self.ctx = ACQ;
    }

    /// Decide everything still undecided (the end of a stream), the last
    /// byte padded with zeros; returns the number of bits decided, counting
    /// from the first this call put in `out`.
    pub fn flush(&mut self, out: &mut Vec<u8>) -> usize {
        let mut bits = 0;
        self.run(out);
        // The last steps: made up to a whole super-block with erasures (no
        // information), the bits kept the real ones. The last of them have
        // no lookahead, as in an unterminated code.
        let rest = self.pending();
        if rest > 0 {
            self.pairs.resize(2 * (self.ctx + 2 * SEGMENT + DEPTH), 0);
            self.super_block(out, rest);
            bits += rest;
        }
        self.reset();
        bits
    }
}

/// Trace two segments back, interleaved (two independent chains of dependent
/// steps, so the loop's work overlaps): from each one's best state through its
/// lookahead (the state only) and then through the [`SEGMENT`] steps whose
/// bits are written, the earliest bit the most significant of byte 0.
fn traceback2(dec: [&[u64]; 2], best: [usize; 2], out: [&mut [u8]; 2]) {
    let (da, db) = (&dec[0][..SEG_STEPS], &dec[1][..SEG_STEPS]);
    let (mut sa, mut sb) = (best[0], best[1]);
    for (&a, &b) in da[ACQ + SEGMENT..].iter().zip(&db[ACQ + SEGMENT..]).rev() {
        sa = back(sa, a);
        sb = back(sb, b);
    }
    let [oa, ob] = out;
    let bytes = oa
        .iter_mut()
        .zip(ob.iter_mut())
        .zip(da[ACQ..ACQ + SEGMENT].as_chunks::<8>().0)
        .zip(db[ACQ..ACQ + SEGMENT].as_chunks::<8>().0)
        .rev();
    for (((byte_a, byte_b), ca), cb) in bytes {
        let (mut va, mut vb) = (0u8, 0u8);
        for i in (0..8).rev() {
            va |= ((sa >> 5) as u8) << (7 - i);
            vb |= ((sb >> 5) as u8) << (7 - i);
            sa = back(sa, ca[i]);
            sb = back(sb, cb[i]);
        }
        *byte_a = va;
        *byte_b = vb;
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
        decode_in(Viterbi::new(rate), soft, scalar)
    }

    fn decode_in(mut v: Viterbi, soft: &[i8], scalar: bool) -> Vec<u8> {
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

    /// Enough bits for several segments and a ragged end.
    const LONG: usize = 7 * 5 * 1200;

    #[test]
    fn noise_free_input_decodes_exactly_at_every_rate() {
        for rate in Rate::ALL {
            let bits = data(LONG, 3);
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
    fn the_avx2_kernels_agree_with_the_portable_ones() {
        for rate in Rate::ALL {
            let bits = data(LONG, 9);
            // Noisy enough for the decoder to make mistakes: equal mistakes.
            let soft = channel(rate, &bits, 40.0, 45.0, 2);
            assert_eq!(
                decode(rate, &soft, false),
                decode(rate, &soft, true),
                "{rate:?}"
            );
        }
    }

    #[test]
    fn the_code_is_decoded_as_well_as_the_float_decoder_does_it() {
        // Bit errors against conv::Viterbi's on the same noisy input. The
        // quantisation and the shorter lookahead may cost a little, and the
        // segments' joins none.
        for rate in [Rate::R1_2, Rate::R2_3, Rate::R3_4, Rate::R7_8] {
            let bits = data(7 * 5 * 3000, 11);
            let sd = match rate {
                Rate::R1_2 => 0.85,
                Rate::R2_3 => 0.7,
                Rate::R3_4 => 0.6,
                _ => 0.5,
            };
            let soft = channel(rate, &bits, 40.0, 40.0 * sd, 5);
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
            let bits = data(LONG, 4);
            let soft = channel(rate, &bits, 40.0, 25.0, 6);
            let whole = decode(rate, &soft, false);
            let mut v = Viterbi::new(rate);
            let mut out = Vec::new();
            let mut s = 77u64;
            let mut i = 0;
            while i < soft.len() {
                let n = (1 + (lcg(&mut s) as usize % 7000)).min(soft.len() - i);
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
        // What matters here is that the decoder neither panics nor loses
        // count: one step per period of three bits.
        assert!(out.len() * 8 >= bits.len() - 16);
    }

    #[test]
    #[ignore = "benchmark"]
    fn viterbi_breakdown() {
        let rate = Rate::R2_3;
        let bits = data(7 * 5 * 20_000, 3);
        let soft = channel(rate, &bits, 40.0, 25.0, 6);
        let mut v = Viterbi::new(rate);
        let t = std::time::Instant::now();
        v.fill(&soft);
        let t_fill = t.elapsed().as_nanos();
        let (mut t_kernel, mut t_trace, mut blocks) = (0u128, 0u128, 0);
        let mut out = Vec::new();
        while v.pending() >= 2 * SEGMENT + DEPTH {
            let ctx = v.ctx;
            let t = std::time::Instant::now();
            let a0 = if ctx >= ACQ { 0 } else { 2 * (ACQ - ctx) };
            let mut padded = vec![0i8; a0];
            padded.extend_from_slice(&v.pairs[..2 * (SEG_STEPS - a0 / 2)]);
            let pa: &[i8] = if ctx >= ACQ {
                &v.pairs[..2 * SEG_STEPS]
            } else {
                &padded
            };
            let b0 = ctx + SEGMENT - ACQ;
            let pb = &v.pairs[2 * b0..2 * (b0 + SEG_STEPS)];
            let (ba, bb) = v.core.run(pa, pb, &mut v.dec_a, &mut v.dec_b);
            t_kernel += t.elapsed().as_nanos();
            let t = std::time::Instant::now();
            let mut o = vec![0u8; 2 * SEGMENT / 8];
            let (oa, ob) = o.split_at_mut(SEGMENT / 8);
            traceback2([&v.dec_a, &v.dec_b], [ba, bb], [oa, ob]);
            t_trace += t.elapsed().as_nanos();
            out.extend_from_slice(&o);
            v.pairs.drain(..2 * (ctx + 2 * SEGMENT - ACQ));
            v.ctx = ACQ;
            blocks += 1;
        }
        let n = (blocks * 2 * SEGMENT) as f64;
        eprintln!(
            "fill {:.2} kernel {:.2} traceback {:.2} ns/bit",
            t_fill as f64 / n,
            t_kernel as f64 / n,
            t_trace as f64 / n
        );
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
                // A symbol at a time, as the receiver pushes it.
                let mut out = Vec::new();
                let t = std::time::Instant::now();
                for piece in soft.chunks(24_192) {
                    v.push(piece, &mut out);
                }
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
