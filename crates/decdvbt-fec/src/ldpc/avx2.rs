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
//!
//! **Registers.** Two blocks at a time are taken through all of a layer's
//! edges, so that each lane's two smallest magnitudes, argmin and sign
//! product stay in registers instead of being loaded and stored for every
//! edge: 58 µs an iteration against 75 for the pass-by-pass version (a chunk
//! of one gave 63, of three 59, of four 61: more accumulators than registers).

use std::arch::x86_64::*;

use super::decoder::{LdpcDecoder, MSG_MAX, POST_MAX};
use super::{Edge, GROUP_STRIDE, LANES};

/// 16-lane blocks in a group, the last one half real.
const BLOCKS: usize = LANES.div_ceil(16);
const _: () = assert!(LANES - (BLOCKS - 1) * 16 == 8);

/// Entries of one edge's deltas, twice over, then room for a block's overshoot.
const DM: usize = 2 * LANES + 16;
/// Blocks a pass works on together (their accumulators stay in registers).
const CHUNK: usize = 2;

/// Working memory of the kernel.
pub(crate) struct Scratch {
    /// The extrinsics of a chunk of blocks, a chunk's width for each edge.
    t: Vec<i16>,
    /// Each edge's new deltas of a layer, twice over (see the module's note).
    dm: Vec<i16>,
}

impl Scratch {
    pub(crate) fn new(max_degree: usize) -> Scratch {
        Scratch {
            t: vec![0; max_degree * CHUNK * 16],
            dm: vec![0; max_degree * DM],
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

/// One chunk of `NB` 16-lane blocks (b0, ...) of a layer: the extrinsics of
/// every edge, the two smallest magnitudes, where the smallest is and the sign
/// product (kept in registers across the edges), then each edge's new message
/// and the change it makes to its group, which goes to `dm`. Returns how many
/// of the chunk's checks the posteriors as read leave unsatisfied (the sign
/// product of the values themselves, which costs one XOR an edge here and a
/// pass over every edge after the iteration otherwise).
///
/// # Safety
/// The CPU must have AVX2; the pointers are the decoder's (`v` the
/// posteriors, `r` the messages, `t` and `dm` the scratch) with `edges` the
/// layer's edges, `start` the first one's number.
#[target_feature(enable = "avx2")]
#[allow(clippy::too_many_arguments)]
unsafe fn chunk<const NB: usize>(
    edges: &[Edge],
    v: *const i16,
    r: *mut i8,
    t: *mut i16,
    dm: *mut i16,
    start: usize,
    corner: bool,
    b0: usize,
    shift: __m128i,
) -> u32 {
    let deg = edges.len();
    unsafe {
        let msg_max = _mm256_set1_epi16(MSG_MAX);
        let zero = _mm256_setzero_si256();
        let (mut m1, mut m2) = ([msg_max; NB], [msg_max; NB]);
        let (mut ar, mut sg) = ([zero; NB], [zero; NB]);
        let mut par = [zero; NB];
        for (e, edge) in edges.iter().enumerate() {
            let s = edge.shift as usize;
            let vg = v.add(edge.group as usize * GROUP_STRIDE);
            let rp = r.add((start + e) * LANES);
            let tp = t.add(e * CHUNK * 16);
            let ev = _mm256_set1_epi16(e as i16);
            let patch = corner && e == deg - 1;
            for j in 0..NB {
                let c0 = (b0 + j) * 16;
                let x = ld(vg.add(rotated_offset(c0, s)));
                let old = _mm256_cvtepi8_epi16(_mm_loadu_si128(rp.add(c0) as *const __m128i));
                let mut tv = _mm256_sub_epi16(x, old);
                // Check 0 has no corner bit: its lane leaves the parity.
                par[j] = _mm256_xor_si256(
                    par[j],
                    if patch && b0 + j == 0 {
                        _mm256_insert_epi16::<0>(x, 0)
                    } else {
                        x
                    },
                );
                if patch && b0 + j == 0 {
                    // A certain 0: changes neither the sign product nor the
                    // minima.
                    tv = _mm256_insert_epi16::<0>(tv, POST_MAX);
                }
                st(tp.add(16 * j), tv);
                let a = _mm256_min_epi16(_mm256_abs_epi16(tv), msg_max);
                let below1 = _mm256_cmpgt_epi16(m1[j], a);
                m2[j] = _mm256_min_epi16(m2[j], _mm256_max_epi16(m1[j], a));
                m1[j] = _mm256_min_epi16(m1[j], a);
                ar[j] = _mm256_blendv_epi8(ar[j], ev, below1);
                sg[j] = _mm256_xor_si256(sg[j], tv);
            }
        }
        // The sign bit of each i16 lane is the odd byte's top bit; the
        // half-real block counts its 8 real lanes.
        let mut unsatisfied = 0;
        for (j, p) in par.iter().enumerate() {
            let real = if b0 + j + 1 < BLOCKS {
                0xAAAA_AAAA
            } else {
                0xAAAA
            };
            unsatisfied += (_mm256_movemask_epi8(*p) as u32 & real).count_ones();
        }
        // Normalized min-sum, in integers: m − m/2^norm_shift.
        for j in 0..NB {
            m1[j] = _mm256_sub_epi16(m1[j], _mm256_sra_epi16(m1[j], shift));
            m2[j] = _mm256_sub_epi16(m2[j], _mm256_sra_epi16(m2[j], shift));
        }
        for (e, _) in edges.iter().enumerate() {
            let rp = r.add((start + e) * LANES);
            let tp = t.add(e * CHUNK * 16);
            let tw = dm.add(e * DM);
            let ev = _mm256_set1_epi16(e as i16);
            let patch = corner && e == deg - 1;
            for j in 0..NB {
                let blk = b0 + j;
                let c0 = blk * 16;
                let tv = ld(tp.add(16 * j));
                // The smallest of the other edges, with the sign of the
                // product without this edge's own.
                let mag = _mm256_blendv_epi8(m1[j], m2[j], _mm256_cmpeq_epi16(ar[j], ev));
                let neg = _mm256_srai_epi16::<15>(_mm256_xor_si256(sg[j], tv));
                let new = _mm256_sub_epi16(_mm256_xor_si256(mag, neg), neg);
                let old = _mm256_cvtepi8_epi16(_mm_loadu_si128(rp.add(c0) as *const __m128i));
                let mut delta = _mm256_sub_epi16(new, old);
                if patch && blk == 0 {
                    delta = _mm256_insert_epi16::<0>(delta, 0);
                }
                let lo = _mm256_castsi256_si128(new);
                if blk + 1 < BLOCKS {
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
        }
        unsatisfied
    }
}

/// One layer of normalized min-sum, as `LdpcDecoder::layer`, and its checks
/// the posteriors left unsatisfied as they were read.
///
/// # Safety
/// The CPU must have AVX2.
#[target_feature(enable = "avx2,popcnt")]
pub(super) unsafe fn layer(d: &mut LdpcDecoder, layer: usize) -> u32 {
    let (start, end) = (d.code.layer_start[layer], d.code.layer_start[layer + 1]);
    // Layer 0's last edge links p_(j−1) as group q−1 shifted by one; lane 0
    // (check 0) has no such bit.
    let corner = layer == 0;
    let edges = &d.code.edges[start..end];
    let sc = &mut d.avx2;
    let (v, r) = (d.v.as_mut_ptr(), d.r.as_mut_ptr());
    let (t, dm) = (sc.t.as_mut_ptr(), sc.dm.as_mut_ptr());

    unsafe {
        // Every edge's extrinsic, then every edge's message, a chunk of blocks
        // at a time: all of the layer's extrinsics are gathered before any
        // posterior is changed.
        let shift = _mm_cvtsi32_si128(d.norm_shift as i32);
        let (mut b, mut unsatisfied) = (0, 0);
        while b + CHUNK < BLOCKS {
            unsatisfied += chunk::<CHUNK>(edges, v, r, t, dm, start, corner, b, shift);
            b += CHUNK;
        }
        while b < BLOCKS {
            unsatisfied += chunk::<1>(edges, v, r, t, dm, start, corner, b, shift);
            b += 1;
        }

        // Add each edge's deltas to its group, in the group's own lane order.
        let post_max = _mm256_set1_epi16(POST_MAX);
        let post_min = _mm256_set1_epi16(-POST_MAX);
        for (e, edge) in edges.iter().enumerate() {
            let s = edge.shift as usize;
            let tw = dm.add(e * DM);
            let vg = v.add(edge.group as usize * GROUP_STRIDE);
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
        unsatisfied
    }
}

/// The information bits' LLRs into the posteriors, a group at a time.
///
/// # Safety
/// The CPU must have AVX2; `llr` holds the groups' 360 bytes each (and 8 more
/// bytes after the last, read and not used); `v` is laid out as the decoder's.
#[target_feature(enable = "avx2")]
pub(super) unsafe fn load_info(v: &mut [i16], llr: &[i8], groups: usize) {
    assert!(llr.len() >= groups * LANES + 8 && v.len() >= groups * GROUP_STRIDE);
    unsafe {
        for g in 0..groups {
            let (src, dst) = (
                llr.as_ptr().add(g * LANES),
                v.as_mut_ptr().add(g * GROUP_STRIDE),
            );
            for b in 0..BLOCKS - 1 {
                let x = _mm256_cvtepi8_epi16(_mm_loadu_si128(src.add(16 * b) as *const __m128i));
                st(dst.add(16 * b), x);
            }
            let i0 = (BLOCKS - 1) * 16;
            let x = _mm_cvtepi8_epi16(_mm_loadl_epi64(src.add(i0) as *const __m128i));
            _mm_storeu_si128(dst.add(i0) as *mut __m128i, x);
        }
    }
}

/// The hard decisions of the information groups as bytes, MSB first: a bit is
/// 1 where the posterior is negative.
///
/// # Safety
/// The CPU must have AVX2; `info` has 45 bytes for each of the `groups`.
#[target_feature(enable = "avx2")]
pub(super) unsafe fn read_out(v: &[i16], groups: usize, info: &mut [u8]) {
    assert!(info.len() >= groups * (LANES / 8) && v.len() >= groups * GROUP_STRIDE);
    unsafe {
        // Each eight lanes in reverse, so that the first lane is a byte's MSB
        // once the sign bits are gathered.
        let reverse8 = _mm_setr_epi8(7, 6, 5, 4, 3, 2, 1, 0, 15, 14, 13, 12, 11, 10, 9, 8);
        for g in 0..groups {
            let src = v.as_ptr().add(g * GROUP_STRIDE);
            let dst = info.as_mut_ptr().add(g * (LANES / 8));
            for b in 0..BLOCKS - 1 {
                let x = ld(src.add(16 * b));
                let packed =
                    _mm_packs_epi16(_mm256_castsi256_si128(x), _mm256_extracti128_si256::<1>(x));
                let m = _mm_movemask_epi8(_mm_shuffle_epi8(packed, reverse8)) as u16;
                let bytes = m.to_le_bytes();
                *dst.add(2 * b) = bytes[0];
                *dst.add(2 * b + 1) = bytes[1];
            }
            let i0 = (BLOCKS - 1) * 16;
            let x = _mm_loadu_si128(src.add(i0) as *const __m128i);
            let m = _mm_movemask_epi8(_mm_shuffle_epi8(_mm_packs_epi16(x, x), reverse8)) as u8;
            *dst.add(2 * (BLOCKS - 1)) = m;
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

    /// The hard decisions come out as the bytes the portable read-out makes.
    #[test]
    fn the_read_out_matches_a_plain_one() {
        if !std::arch::is_x86_feature_detected!("avx2") {
            return;
        }
        let groups = 3;
        let mut next = rng(9);
        let mut v = vec![0i16; groups * GROUP_STRIDE + 16];
        for x in v.iter_mut() {
            *x = (next() % 2001) as i16 - 1000;
        }
        let mut fast = vec![0u8; groups * 45];
        // SAFETY: AVX2 was checked.
        unsafe { read_out(&v, groups, &mut fast) };
        let mut plain = vec![0u8; groups * 45];
        for g in 0..groups {
            for c in 0..LANES {
                if v[g * GROUP_STRIDE + c] < 0 {
                    let bit = g * LANES + c;
                    plain[bit / 8] |= 0x80 >> (bit % 8);
                }
            }
        }
        // One negative lane: its place in the bytes.
        for lane in [0usize, 7, 8, 9, 15, 16, 351, 359] {
            let mut one = vec![1i16; GROUP_STRIDE + 16];
            one[lane] = -1;
            let mut o = vec![0u8; 45];
            // SAFETY: AVX2 was checked.
            unsafe { read_out(&one, 1, &mut o) };
            let mut want = vec![0u8; 45];
            want[lane / 8] = 0x80 >> (lane % 8);
            assert_eq!(o, want, "lane {lane}");
        }
        assert_eq!(fast, plain);
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
