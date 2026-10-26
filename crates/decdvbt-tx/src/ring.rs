// made by CasualArclamp
//! The two bounded FIFOs between the threads.
//!
//! - [`TsRing`] holds transport packets from ffmpeg. Its producer (the pipe
//!   reader) **blocks when it is full**, which stops reading the pipe, which
//!   stops ffmpeg: that is the throttle. Its consumer (the modulator thread)
//!   **never blocks**: if it is empty the caller sends null packets.
//! - [`IqRing`] holds finished cs8 blocks. Its producer (the modulator thread)
//!   blocks when it is full. Its consumer (the radio's USB thread) **never
//!   blocks**: empty means an underrun, counted, and zeros go out.
//!
//! One `Mutex` and a `Condvar` each. The locks are taken once per 64 KiB
//! (TS) or once per 128 KiB block (IQ), a few hundred times a second, so
//! contention does not matter; lock-free would only add risk.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};

use decdvbt_core::Iq;

use decdvbt_mod::ts::TS_LEN;

struct TsInner {
    bytes: VecDeque<u8>,
    /// The producer has finished (end of file); drain what is left.
    closed: bool,
    /// Stop everything now.
    aborted: bool,
}

/// Whole transport packets, oldest first.
pub struct TsRing {
    inner: Mutex<TsInner>,
    space: Condvar,
    capacity: usize,
}

impl TsRing {
    /// A ring that holds `capacity_packets` packets.
    pub fn new(capacity_packets: usize) -> TsRing {
        TsRing {
            inner: Mutex::new(TsInner {
                bytes: VecDeque::with_capacity(capacity_packets * TS_LEN),
                closed: false,
                aborted: false,
            }),
            space: Condvar::new(),
            capacity: capacity_packets * TS_LEN,
        }
    }

    /// Append whole packets (`data.len()` a multiple of 188), waiting for
    /// room. `false` if the ring was aborted (the caller stops).
    pub fn push_blocking(&self, data: &[u8]) -> bool {
        debug_assert_eq!(data.len() % TS_LEN, 0);
        let mut g = self.inner.lock().unwrap();
        let mut rest = data;
        while !rest.is_empty() {
            while !g.aborted && g.bytes.len() >= self.capacity {
                g = self.space.wait(g).unwrap();
            }
            if g.aborted {
                return false;
            }
            // Whole packets only, so the consumer never sees half of one.
            let room = (self.capacity - g.bytes.len()) / TS_LEN * TS_LEN;
            let n = room.min(rest.len());
            g.bytes.extend(&rest[..n]);
            rest = &rest[n..];
        }
        true
    }

    /// Move up to `out.len() / 188` packets into `out` without waiting;
    /// returns how many.
    pub fn pop(&self, out: &mut [u8]) -> usize {
        let mut g = self.inner.lock().unwrap();
        let n = (g.bytes.len().min(out.len()) / TS_LEN) * TS_LEN;
        // A VecDeque is two slices; copy from each, then drop what was taken.
        let (a, b) = g.bytes.as_slices();
        let from_a = n.min(a.len());
        out[..from_a].copy_from_slice(&a[..from_a]);
        out[from_a..n].copy_from_slice(&b[..n - from_a]);
        g.bytes.drain(..n);
        drop(g);
        self.space.notify_all();
        n / TS_LEN
    }

    /// The producer is done (end of file): once drained, [`Self::finished`].
    pub fn close(&self) {
        self.inner.lock().unwrap().closed = true;
    }

    /// Stop now: wakes a blocked producer, which returns `false`.
    pub fn abort(&self) {
        self.inner.lock().unwrap().aborted = true;
        self.space.notify_all();
    }

    /// Closed and empty.
    pub fn finished(&self) -> bool {
        let g = self.inner.lock().unwrap();
        g.closed && g.bytes.is_empty()
    }

    pub fn len_packets(&self) -> usize {
        self.inner.lock().unwrap().bytes.len() / TS_LEN
    }

    pub fn capacity_packets(&self) -> usize {
        self.capacity / TS_LEN
    }

    /// 0.0 to 1.0.
    pub fn fill(&self) -> f32 {
        self.len_packets() as f32 / self.capacity_packets().max(1) as f32
    }
}

struct IqInner {
    blocks: VecDeque<Vec<u8>>,
    pool: Vec<Vec<u8>>,
    aborted: bool,
}

/// Finished cs8 blocks, oldest first, with a pool of empty `Vec`s so the
/// steady state allocates nothing.
pub struct IqRing {
    inner: Mutex<IqInner>,
    space: Condvar,
    depth: usize,
}

impl IqRing {
    /// A ring of at most `depth` blocks.
    pub fn new(depth: usize) -> IqRing {
        IqRing {
            inner: Mutex::new(IqInner {
                blocks: VecDeque::with_capacity(depth),
                pool: Vec::new(),
                aborted: false,
            }),
            space: Condvar::new(),
            depth,
        }
    }

    /// An empty block to fill: a recycled one if there is one.
    pub fn get_block(&self) -> Vec<u8> {
        self.inner.lock().unwrap().pool.pop().unwrap_or_default()
    }

    /// Queue a block, waiting for room. `false` if aborted.
    pub fn push_blocking(&self, block: Vec<u8>) -> bool {
        let mut g = self.inner.lock().unwrap();
        while !g.aborted && g.blocks.len() >= self.depth {
            g = self.space.wait(g).unwrap();
        }
        if g.aborted {
            return false;
        }
        g.blocks.push_back(block);
        true
    }

    /// The oldest block, without waiting.
    pub fn try_pop(&self) -> Option<Vec<u8>> {
        let b = self.inner.lock().unwrap().blocks.pop_front();
        if b.is_some() {
            self.space.notify_all();
        }
        b
    }

    /// Give a drained block back for reuse.
    pub fn recycle(&self, mut block: Vec<u8>) {
        block.clear();
        let mut g = self.inner.lock().unwrap();
        if g.pool.len() < self.depth + 2 {
            g.pool.push(block);
        }
    }

    pub fn abort(&self) {
        self.inner.lock().unwrap().aborted = true;
        self.space.notify_all();
    }

    pub fn len_blocks(&self) -> usize {
        self.inner.lock().unwrap().blocks.len()
    }

    /// 0.0 to 1.0.
    pub fn fill(&self) -> f32 {
        self.len_blocks() as f32 / self.depth.max(1) as f32
    }
}

/// Pack gained samples as cs8: `round(x * 128)` saturating to i8, I then Q.
/// The same conversion as `IqFileWriter` (`decdvbt-io/src/file.rs`) except
/// that an exact tie (x·128 = k + ½, which a continuous signal never makes) is
/// rounded to even instead of away from zero. Returns how many samples clipped
/// (a component rounding to ±128 or beyond).
pub fn pack_cs8(samples: &[Iq], out: &mut Vec<u8>) -> u64 {
    pack_cs8_scaled(samples, 128.0, out)
}

/// [`pack_cs8`] with the gain folded in: `round(x * scale)`, so `scale` is
/// 128 times the linear gain. One pass over the samples instead of a gain pass
/// and a pack pass; on x86-64 eight samples at a time in SSE2 (always there),
/// elsewhere a plain loop with the same results.
pub fn pack_cs8_scaled(samples: &[Iq], scale: f32, out: &mut Vec<u8>) -> u64 {
    let start = out.len();
    out.resize(start + samples.len() * 2, 0);
    let dst = &mut out[start..];
    #[cfg(target_arch = "x86_64")]
    let done = pack_sse2(samples, scale, dst);
    #[cfg(not(target_arch = "x86_64"))]
    let done = (0, 0);
    let (n, mut clipped) = done;
    for (s, d) in samples[n..]
        .iter()
        .zip(dst[2 * n..].as_chunks_mut::<2>().0.iter_mut())
    {
        // `as i8` on a float saturates, so the clamp is done for us.
        let (i, q) = (
            (s.re * scale).round_ties_even(),
            (s.im * scale).round_ties_even(),
        );
        clipped += (i.abs() >= 128.0 || q.abs() >= 128.0) as u64;
        *d = [i as i8 as u8, q as i8 as u8];
    }
    clipped
}

/// The whole groups of 8 samples of [`pack_cs8_scaled`]: returns how many
/// samples it did and how many of them clipped.
#[cfg(target_arch = "x86_64")]
fn pack_sse2(samples: &[Iq], scale: f32, dst: &mut [u8]) -> (usize, u64) {
    use std::arch::x86_64::*;
    let n = samples.len() / 8 * 8;
    // `Iq` is two `f32`s, re then im (`#[repr(C)]` in num-complex), so the
    // samples are a flat run of floats.
    let floats: &[f32] =
        // SAFETY: the pointer and twice the length cover exactly the samples.
        unsafe { std::slice::from_raw_parts(samples.as_ptr().cast::<f32>(), samples.len() * 2) };
    let mut clipped = 0u64;
    // SAFETY: SSE2 is part of x86-64. Every load reads 4 floats inside
    // `floats[..2n]` and the store writes 16 bytes inside `dst[..2n]`.
    unsafe {
        let k = _mm_set1_ps(scale);
        // Lanes 0 and 2 of a group are the samples' real parts.
        let first_of_pair = _mm_set_epi32(0, -1, 0, -1);
        let (hi, lo) = (_mm_set1_epi32(127), _mm_set1_epi32(-127));
        let mut count = _mm_setzero_si128();
        for g in 0..n / 8 {
            let p = floats.as_ptr().add(g * 16);
            // Round to nearest even (the default mode) to 32-bit integers.
            let v: [__m128i; 4] =
                std::array::from_fn(|i| _mm_cvtps_epi32(_mm_mul_ps(_mm_loadu_ps(p.add(4 * i)), k)));
            // Beyond 127 or below −127 rounds to ±128 or more: clipped. A
            // sample counts once however many of its components do.
            for x in v {
                let over = _mm_or_si128(_mm_cmpgt_epi32(x, hi), _mm_cmplt_epi32(x, lo));
                let sample = _mm_or_si128(over, _mm_srli_epi64::<32>(over));
                count = _mm_sub_epi32(count, _mm_and_si128(sample, first_of_pair));
            }
            // 4 x 4 i32 to 16 i8, saturating.
            let a = _mm_packs_epi32(v[0], v[1]);
            let b = _mm_packs_epi32(v[2], v[3]);
            _mm_storeu_si128(dst.as_mut_ptr().add(g * 16).cast(), _mm_packs_epi16(a, b));
        }
        let mut lanes = [0i32; 4];
        _mm_storeu_si128(lanes.as_mut_ptr().cast(), count);
        clipped += lanes.iter().map(|&c| c as u64).sum::<u64>();
    }
    (n, clipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_ring_keeps_whole_packets_in_order() {
        let r = TsRing::new(4);
        let data: Vec<u8> = (0..3 * TS_LEN).map(|i| i as u8).collect();
        assert!(r.push_blocking(&data));
        let mut out = vec![0u8; 2 * TS_LEN + 50];
        assert_eq!(r.pop(&mut out), 2);
        assert_eq!(&out[..2 * TS_LEN], &data[..2 * TS_LEN]);
        assert_eq!(r.len_packets(), 1);
        r.close();
        assert!(!r.finished());
        assert_eq!(r.pop(&mut out), 1);
        assert!(r.finished());
    }

    #[test]
    fn a_full_ts_ring_blocks_until_popped_or_aborted() {
        let r = std::sync::Arc::new(TsRing::new(2));
        assert!(r.push_blocking(&vec![0x47; 2 * TS_LEN]));
        let r2 = r.clone();
        let t = std::thread::spawn(move || r2.push_blocking(&[0x47; TS_LEN]));
        std::thread::sleep(std::time::Duration::from_millis(50));
        r.abort();
        assert!(!t.join().unwrap());
    }

    #[test]
    fn iq_ring_recycles_blocks() {
        let r = IqRing::new(2);
        let mut b = r.get_block();
        b.extend_from_slice(&[1, 2, 3]);
        assert!(r.push_blocking(b));
        let b = r.try_pop().unwrap();
        assert_eq!(b, [1, 2, 3]);
        r.recycle(b);
        assert!(r.get_block().is_empty());
        assert!(r.try_pop().is_none());
    }

    /// The plain loop the vector code must agree with, byte for byte.
    fn pack_plain(samples: &[Iq], scale: f32) -> (Vec<u8>, u64) {
        let (mut out, mut clipped) = (Vec::new(), 0u64);
        for s in samples {
            let (i, q) = (
                (s.re * scale).round_ties_even(),
                (s.im * scale).round_ties_even(),
            );
            clipped += (i.abs() >= 128.0 || q.abs() >= 128.0) as u64;
            out.push(i as i8 as u8);
            out.push(q as i8 as u8);
        }
        (out, clipped)
    }

    #[test]
    fn the_vector_pack_agrees_with_the_plain_one_with_gain_and_clipping() {
        let mut seed = 7u32;
        let mut next = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        for n in [0usize, 1, 7, 8, 9, 63, 64, 1000, 4097] {
            // Mostly in range, some beyond full scale on either side.
            let s: Vec<Iq> = (0..n)
                .map(|_| Iq::new(next() * 3.0, next() * 3.0))
                .collect();
            for scale in [128.0f32, 128.0 * 0.241, 40.0] {
                let mut out = vec![0xAAu8; 3]; // appends after what is there
                let clipped = pack_cs8_scaled(&s, scale, &mut out);
                let (want, want_clipped) = pack_plain(&s, scale);
                assert_eq!(&out[..3], &[0xAA; 3]);
                assert_eq!(&out[3..], &want[..], "n {n} scale {scale}");
                assert_eq!(clipped, want_clipped, "n {n} scale {scale}");
            }
        }
    }

    #[test]
    fn a_sample_with_both_components_clipped_counts_once() {
        let s = vec![Iq::new(2.0, -2.0); 8];
        let mut out = Vec::new();
        assert_eq!(pack_cs8(&s, &mut out), 8);
        assert_eq!(&out[..2], &[127, 128]);
    }

    #[test]
    fn cs8_matches_the_file_writer() {
        let s = [Iq::new(0.5, -0.5), Iq::new(2.0, -2.0), Iq::new(0.0039, 0.0)];
        let mut out = Vec::new();
        let clipped = pack_cs8(&s, &mut out);
        assert_eq!(out, [64, 192, 127, 128, 0, 0]);
        assert_eq!(clipped, 1);
    }
}
