// made by CasualArclamp
//! The layered min-sum decoder's AVX2 kernel: [`decoder`](super::decoder)'s
//! arithmetic written with intrinsics, 16 lanes of i16 to a register.
//!
//! Why by hand: the portable loops are written to be vectorised, and the
//! compiler leaves them scalar (about ten cycles a lane, measured on a
//! Zen 2). This kernel does the same sums in a tenth of the time and gives the
//! same decisions bit for bit (`kernel_matches_portable`).
//!
//! **Rotations.** A check lane `c` sees bit `(c − s) mod 360` of its group.
//! Read as 16-lane blocks, that is one contiguous run of the group starting at
//! `c₀ − s` (or `c₀ − s + 360` when the block lies below the shift), and a
//! block that straddles the wrap reads on into a 16-entry copy of the group's
//! head that follows each group (`GROUP_STRIDE`). The way back, adding a
//! check-lane delta to the posteriors, reads the delta from a doubled buffer
//! instead: `d'[i] = delta[(i + s) mod 360] = twice[i + s]`.
//!
//! 360 = 22 × 16 + 8: the 23rd block is half real. Its extra lanes compute
//! garbage that is never stored (the stores of that block are 8 lanes wide).
//!
//! The layers' semantics follow the portable code exactly: every extrinsic of
//! a layer is gathered before any posterior is updated, and updates are
//! differences.

use std::arch::x86_64::*;

use super::decoder::{LdpcDecoder, MSG_MAX, POST_MAX};
use super::{GROUP_STRIDE, LANES};

/// 16-lane blocks in a group, the last one half real.
const BLOCKS: usize = LANES.div_ceil(16);
/// A block-padded group.
const PADDED: usize = BLOCKS * 16;
const _: () = assert!(LANES - (BLOCKS - 1) * 16 == 8);

/// Working memory of the kernel.
pub(crate) struct Scratch {
    /// The extrinsics of a layer, one padded group per edge.
    t: Vec<i16>,
    min1: [i16; PADDED],
    min2: [i16; PADDED],
    arg: [i16; PADDED],
    sign: [i16; PADDED],
    /// A layer's new deltas, twice over, then room for a block's overshoot.
    twice: [i16; 2 * LANES + 16],
}

impl Scratch {
    pub(crate) fn new(max_degree: usize) -> Scratch {
        Scratch {
            t: vec![0; max_degree * PADDED],
            min1: [0; PADDED],
            min2: [0; PADDED],
            arg: [0; PADDED],
            sign: [0; PADDED],
            twice: [0; 2 * LANES + 16],
        }
    }
}

#[inline]
#[target_feature(enable = "avx2")]
unsafe fn ld(p: *const i16) -> __m256i {
    unsafe { _mm256_loadu_si256(p as *const __m256i) }
}

#[inline]
#[target_feature(enable = "avx2")]
unsafe fn st(p: *mut i16, v: __m256i) {
    unsafe { _mm256_storeu_si256(p as *mut __m256i, v) }
}

/// The 16 entries of a group a block of check lanes reads, `c0` the block's
/// first lane and `s` the edge's shift.
#[inline]
fn rotated_offset(c0: usize, s: usize) -> usize {
    if c0 >= s { c0 - s } else { c0 + LANES - s }
}

/// One layer of normalized min-sum, as `LdpcDecoder::layer`.
///
/// # Safety
/// The CPU must have AVX2.
#[target_feature(enable = "avx2,popcnt")]
pub(super) unsafe fn layer(d: &mut LdpcDecoder, layer: usize) {
    let (start, end) = (d.code.layer_start[layer], d.code.layer_start[layer + 1]);
    let deg = end - start;
    // Layer 0's last edge links p_(j−1) as group q−1 shifted by one; lane 0
    // (check 0) has no such bit.
    let corner = layer == 0;
    let sc = &mut d.avx2;
    let msg_max = _mm256_set1_epi16(MSG_MAX);
    let zero = _mm256_setzero_si256();

    unsafe {
        for b in 0..BLOCKS {
            st(sc.min1.as_mut_ptr().add(b * 16), msg_max);
            st(sc.min2.as_mut_ptr().add(b * 16), msg_max);
            st(sc.arg.as_mut_ptr().add(b * 16), zero);
            st(sc.sign.as_mut_ptr().add(b * 16), zero);
        }

        // Pass 1: every edge's extrinsic, the two smallest magnitudes and
        // where the smallest is, and the sign product.
        for e in 0..deg {
            let edge = d.code.edges[start + e];
            let s = edge.shift as usize;
            let vg = d.v.as_ptr().add(edge.group as usize * GROUP_STRIDE);
            let rp = d.r.as_ptr().add((start + e) * LANES);
            let tp = sc.t.as_mut_ptr().add(e * PADDED);
            let ev = _mm256_set1_epi16(e as i16);
            let patch = corner && e == deg - 1;
            for b in 0..BLOCKS {
                let c0 = b * 16;
                let v = ld(vg.add(rotated_offset(c0, s)));
                let old = _mm256_cvtepi8_epi16(_mm_loadu_si128(rp.add(c0) as *const __m128i));
                let mut t = _mm256_sub_epi16(v, old);
                if patch && b == 0 {
                    // A certain 0: changes neither the sign product nor the
                    // minima.
                    t = _mm256_insert_epi16::<0>(t, POST_MAX);
                }
                st(tp.add(c0), t);
                let a = _mm256_min_epi16(_mm256_abs_epi16(t), msg_max);
                let m1 = ld(sc.min1.as_ptr().add(c0));
                let m2 = ld(sc.min2.as_ptr().add(c0));
                let ar = ld(sc.arg.as_ptr().add(c0));
                let sg = ld(sc.sign.as_ptr().add(c0));
                let below1 = _mm256_cmpgt_epi16(m1, a);
                st(
                    sc.min2.as_mut_ptr().add(c0),
                    _mm256_min_epi16(m2, _mm256_max_epi16(m1, a)),
                );
                st(sc.min1.as_mut_ptr().add(c0), _mm256_min_epi16(m1, a));
                st(
                    sc.arg.as_mut_ptr().add(c0),
                    _mm256_blendv_epi8(ar, ev, below1),
                );
                st(sc.sign.as_mut_ptr().add(c0), _mm256_xor_si256(sg, t));
            }
        }
        // Normalized min-sum, in integers: m − m/2^norm_shift.
        let shift = _mm_cvtsi32_si128(d.norm_shift as i32);
        for b in 0..BLOCKS {
            let c0 = b * 16;
            let m1 = ld(sc.min1.as_ptr().add(c0));
            let m2 = ld(sc.min2.as_ptr().add(c0));
            st(
                sc.min1.as_mut_ptr().add(c0),
                _mm256_sub_epi16(m1, _mm256_sra_epi16(m1, shift)),
            );
            st(
                sc.min2.as_mut_ptr().add(c0),
                _mm256_sub_epi16(m2, _mm256_sra_epi16(m2, shift)),
            );
        }

        // Pass 2: the new messages and the change each makes to its group.
        let post_max = _mm256_set1_epi16(POST_MAX);
        let post_min = _mm256_set1_epi16(-POST_MAX);
        for e in 0..deg {
            let edge = d.code.edges[start + e];
            let s = edge.shift as usize;
            let rp = d.r.as_mut_ptr().add((start + e) * LANES);
            let tp = sc.t.as_ptr().add(e * PADDED);
            let tw = sc.twice.as_mut_ptr();
            let ev = _mm256_set1_epi16(e as i16);
            let patch = corner && e == deg - 1;
            for b in 0..BLOCKS {
                let c0 = b * 16;
                let t = ld(tp.add(c0));
                let m1 = ld(sc.min1.as_ptr().add(c0));
                let m2 = ld(sc.min2.as_ptr().add(c0));
                let ar = ld(sc.arg.as_ptr().add(c0));
                let sg = ld(sc.sign.as_ptr().add(c0));
                // The smallest of the other edges, with the sign of the
                // product without this edge's own.
                let mag = _mm256_blendv_epi8(m1, m2, _mm256_cmpeq_epi16(ar, ev));
                let neg = _mm256_srai_epi16::<15>(_mm256_xor_si256(sg, t));
                let new = _mm256_sub_epi16(_mm256_xor_si256(mag, neg), neg);
                let old = _mm256_cvtepi8_epi16(_mm_loadu_si128(rp.add(c0) as *const __m128i));
                let mut delta = _mm256_sub_epi16(new, old);
                if patch && b == 0 {
                    delta = _mm256_insert_epi16::<0>(delta, 0);
                }
                let lo = _mm256_castsi256_si128(new);
                if b + 1 < BLOCKS {
                    let hi = _mm256_extracti128_si256::<1>(new);
                    _mm_storeu_si128(rp.add(c0) as *mut __m128i, _mm_packs_epi16(lo, hi));
                    st(tw.add(c0), delta);
                    st(tw.add(LANES + c0), delta);
                } else {
                    // The half-real block: 8 lanes only.
                    _mm_storel_epi64(rp.add(c0) as *mut __m128i, _mm_packs_epi16(lo, lo));
                    let dl = _mm256_castsi256_si128(delta);
                    _mm_storeu_si128(tw.add(c0) as *mut __m128i, dl);
                    _mm_storeu_si128(tw.add(LANES + c0) as *mut __m128i, dl);
                }
            }
            // Add the deltas to the group, in its own lane order.
            let vg = d.v.as_mut_ptr().add(edge.group as usize * GROUP_STRIDE);
            for b in 0..BLOCKS - 1 {
                let i0 = b * 16;
                let sum = _mm256_add_epi16(ld(vg.add(i0)), ld(tw.add(i0 + s)));
                let sum = _mm256_min_epi16(_mm256_max_epi16(sum, post_min), post_max);
                st(vg.add(i0), sum);
                if b == 0 {
                    st(vg.add(LANES), sum);
                }
            }
            let i0 = (BLOCKS - 1) * 16;
            let sum = _mm_add_epi16(
                _mm_loadu_si128(vg.add(i0) as *const __m128i),
                _mm_loadu_si128(tw.add(i0 + s) as *const __m128i),
            );
            let sum = _mm_min_epi16(
                _mm_max_epi16(sum, _mm256_castsi256_si128(post_min)),
                _mm256_castsi256_si128(post_max),
            );
            _mm_storeu_si128(vg.add(i0) as *mut __m128i, sum);
        }
    }
}

/// How many parity checks the hard decisions leave unsatisfied, as
/// `LdpcDecoder::unsatisfied`.
///
/// # Safety
/// The CPU must have AVX2 and POPCNT.
#[target_feature(enable = "avx2,popcnt")]
pub(super) unsafe fn unsatisfied(d: &LdpcDecoder) -> usize {
    let mut count = 0usize;
    unsafe {
        let mut parity = [_mm256_setzero_si256(); BLOCKS];
        for layer in 0..d.code.q {
            let (start, end) = (d.code.layer_start[layer], d.code.layer_start[layer + 1]);
            parity.fill(_mm256_setzero_si256());
            for e in start..end {
                let edge = d.code.edges[e];
                let s = edge.shift as usize;
                let vg = d.v.as_ptr().add(edge.group as usize * GROUP_STRIDE);
                let patch = layer == 0 && e == end - 1;
                for (b, p) in parity.iter_mut().enumerate() {
                    let c0 = b * 16;
                    let mut x = ld(vg.add(rotated_offset(c0, s)));
                    if patch && b == 0 {
                        x = _mm256_insert_epi16::<0>(x, 0);
                    }
                    *p = _mm256_xor_si256(*p, x);
                }
            }
            // The sign bit of each i16 lane is the odd byte's top bit.
            for p in &parity[..BLOCKS - 1] {
                count += (_mm256_movemask_epi8(*p) as u32 & 0xAAAA_AAAA).count_ones() as usize;
            }
            count +=
                (_mm256_movemask_epi8(parity[BLOCKS - 1]) as u32 & 0xAAAA).count_ones() as usize;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::super::LdpcCode;
    use super::super::tests::rng;
    use super::*;
    use crate::params::{FecParams, all_s2};
    use decdvbt_core::{CodeRate, FecFrame};

    fn noisy_llrs(code: &LdpcCode, ebn0_db: f64, rate: f64, seed: u64) -> Vec<i8> {
        let mut next = rng(seed);
        let info: Vec<u8> = (0..code.k / 8).map(|_| next() as u8).collect();
        let mut cw = info.clone();
        cw.resize(code.n / 8, 0);
        code.encode(&info, &mut cw[code.k / 8..]);
        let sigma = (1.0 / (2.0 * rate * 10f64.powf(ebn0_db / 10.0))).sqrt();
        let mut u = rng(seed ^ 0x55);
        let mut gauss = move || {
            let a = ((u() >> 11) as f64 / (1u64 << 53) as f64).max(1e-300);
            let b = (u() >> 11) as f64 / (1u64 << 53) as f64;
            (-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()
        };
        (0..code.n)
            .map(|i| {
                let bit = (cw[i / 8] >> (7 - i % 8)) & 1;
                let y = if bit == 0 { 1.0 } else { -1.0 } + sigma * gauss();
                (2.0 * y / (sigma * sigma) * 4.0)
                    .round()
                    .clamp(-127.0, 127.0) as i8
            })
            .collect()
    }

    /// The kernel and the portable code reach the same decisions in the same
    /// number of iterations, on codes of both families and on frames that
    /// converge, stall or run out of iterations.
    #[test]
    fn kernel_matches_portable() {
        if !(std::arch::is_x86_feature_detected!("avx2")
            && std::arch::is_x86_feature_detected!("popcnt"))
        {
            return;
        }
        let mut codes: Vec<FecParams> = all_s2().take(6).collect();
        for (frame, rate) in [
            (FecFrame::Normal, CodeRate::new(2, 3)),
            (FecFrame::Short, CodeRate::new(3, 5)),
            (FecFrame::Normal, CodeRate::new(3, 5)),
        ] {
            codes.push(FecParams::new_t2(frame, rate).unwrap());
        }
        for p in codes {
            let r = p.rate.num as f64 / p.rate.den as f64;
            for (seed, offset) in [(1u64, 0.3), (2, -0.5), (3, 1.5), (4, -2.5)] {
                let code = LdpcCode::new(p.ldpc_table());
                let (k, n) = (code.k, code.n);
                let ebn0 = if r <= 0.5 {
                    2.0
                } else if r <= 0.75 {
                    3.0
                } else {
                    4.5
                } + offset;
                let llr = noisy_llrs(&code, ebn0, r, seed);
                let mut a = LdpcDecoder::new(code);
                let mut b = LdpcDecoder::new(LdpcCode::new(p.ldpc_table()));
                let (mut ia, mut ib) = (vec![0u8; k / 8], vec![0u8; k / 8]);
                let oa = a.decode_portable(&llr, &mut ia, 30);
                // SAFETY: the CPU features were checked above.
                let ob = unsafe { b.decode_budget_avx2(&llr, &mut ib, 30, false) };
                assert_eq!(oa, ob, "{p:?} seed {seed}: outcome");
                assert_eq!(ia, ib, "{p:?} seed {seed}: decisions");
                for g in 0..n / LANES {
                    assert_eq!(
                        a.v[g * GROUP_STRIDE..][..LANES],
                        b.v[g * GROUP_STRIDE..][..LANES],
                        "{p:?} seed {seed}: group {g}"
                    );
                }
            }
        }
    }
}
