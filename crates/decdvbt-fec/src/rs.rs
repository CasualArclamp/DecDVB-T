// made by CasualArclamp
//! Reed–Solomon codes over GF(256), shortened: DVB-S's RS(204,188, t = 8)
//! (EN 300 421 §4.4.2) and the Intelsat family (126/112, 219/201, …) that
//! satellite modems pair with Viterbi decoding.
//!
//! Field: p(x) = x⁸ + x⁴ + x³ + x² + 1 (0x11D). Generator: the 2t roots
//! λ⁰ … λ^(2t−1) of λ = 02h (first consecutive root 0) — the DVB form,
//! matching `gr-dtv`'s RS encoder (fcr 0, prim 1). A shortened code is the
//! full (255, 255 − 2t) code with leading zero bytes left out.
//!
//! Decoding: syndromes, Berlekamp–Massey, Chien search, Forney.

/// GF(256) log/antilog tables for p(x) = 0x11D.
struct Gf {
    exp: [u8; 512],
    log: [u8; 256],
}

impl Gf {
    const fn new() -> Gf {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        let mut i = 0;
        while i < 255 {
            exp[i] = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11D;
            }
            i += 1;
        }
        while i < 512 {
            exp[i] = exp[i - 255];
            i += 1;
        }
        Gf { exp, log }
    }

    #[inline]
    fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
        }
    }

    #[inline]
    fn div(&self, a: u8, b: u8) -> u8 {
        if a == 0 {
            0
        } else {
            self.exp[(self.log[a as usize] as usize + 255 - self.log[b as usize] as usize) % 255]
        }
    }

    /// α^i.
    #[inline]
    fn pow(&self, i: usize) -> u8 {
        self.exp[i % 255]
    }
}

static GF: Gf = Gf::new();

/// Why a block could not be corrected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RsUncorrectable;

/// One shortened RS code.
#[derive(Debug, Clone)]
pub struct ReedSolomon {
    /// Codeword and message bytes as sent.
    pub n: usize,
    pub k: usize,
    /// Parity bytes, 2t.
    p: usize,
    /// Generator coefficients, g[0] the x^p term's (= 1), highest first.
    g: Vec<u8>,
    /// `times[i][x]` = x·α^i: one lookup a byte per syndrome, for blocks that
    /// are not clean.
    times: Vec<[u8; 256]>,
    /// For 16 parity bytes: `rows[f]` = f·g_i for the 16 generator
    /// coefficients, so one step of the encoder's shift register is a load and
    /// an exclusive-or of 16 bytes. Running it over a block's message and
    /// comparing the result with the block's parity is the receiver's hot path
    /// (most blocks are clean and cost nothing else): about 0.5 µs.
    rows: Vec<[u8; 16]>,
}

impl ReedSolomon {
    /// RS(n, k), shortened from (255, 255 − (n − k)).
    ///
    /// # Panics
    /// If n > 255 or n − k is odd.
    pub fn new(n: usize, k: usize) -> Self {
        assert!(n <= 255 && k < n && (n - k).is_multiple_of(2));
        let p = n - k;
        // g(x) = Π (x − α^i), i = 0..p, coefficients highest first.
        let mut g = vec![1u8];
        for i in 0..p {
            let root = GF.pow(i);
            let mut next = vec![0u8; g.len() + 1];
            for (j, &c) in g.iter().enumerate() {
                next[j] ^= c;
                next[j + 1] ^= GF.mul(c, root);
            }
            g = next;
        }
        let times = (0..p)
            .map(|i| {
                let a = GF.pow(i);
                std::array::from_fn(|x| GF.mul(x as u8, a))
            })
            .collect();
        let rows = if p == 16 {
            (0..256usize)
                .map(|f| std::array::from_fn(|i| GF.mul(f as u8, g[1 + i])))
                .collect()
        } else {
            Vec::new()
        };
        ReedSolomon {
            n,
            k,
            p,
            g,
            times,
            rows,
        }
    }

    /// Whether `block` is a codeword, by the encoder's shift register (only for
    /// 16 parity bytes; `None` for other codes).
    #[cfg(target_arch = "x86_64")]
    fn is_codeword(&self, block: &[u8]) -> Option<bool> {
        use std::arch::x86_64::*;
        if self.rows.is_empty() {
            return None;
        }
        // SAFETY: SSE2 is part of x86-64; the loads are of the 16 bytes of a
        // row and of the 16 parity bytes (the block is `n` long).
        unsafe {
            let mut r = _mm_setzero_si128();
            for &m in &block[..self.k] {
                let fb = (m ^ _mm_cvtsi128_si32(r) as u8) as usize;
                r = _mm_xor_si128(
                    _mm_srli_si128::<1>(r),
                    _mm_loadu_si128(self.rows.as_ptr().add(fb).cast()),
                );
            }
            let parity = _mm_loadu_si128(block[self.k..].as_ptr().cast());
            Some(_mm_movemask_epi8(_mm_cmpeq_epi8(r, parity)) == 0xFFFF)
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn is_codeword(&self, _block: &[u8]) -> Option<bool> {
        None
    }

    /// DVB-S's RS(204, 188).
    pub fn dvb() -> Self {
        Self::new(204, 188)
    }

    /// Correctable byte errors.
    pub fn t(&self) -> usize {
        self.p / 2
    }

    /// Fill the parity bytes after the `k` message bytes of `block`.
    pub fn encode(&self, block: &mut [u8]) {
        assert_eq!(block.len(), self.n);
        let mut rem = vec![0u8; self.p];
        for &m in &block[..self.k] {
            let fb = m ^ rem[0];
            rem.rotate_left(1);
            rem[self.p - 1] = 0;
            if fb != 0 {
                for (r, &g) in rem.iter_mut().zip(&self.g[1..]) {
                    *r ^= GF.mul(fb, g);
                }
            }
        }
        block[self.k..].copy_from_slice(&rem);
    }

    /// Correct `block` in place; returns the bytes corrected.
    pub fn decode(&self, block: &mut [u8]) -> Result<usize, RsUncorrectable> {
        assert_eq!(block.len(), self.n);
        if self.is_codeword(block) == Some(true) {
            return Ok(0);
        }
        // Syndromes S_i = c(α^i), i = 0..p (Horner, highest degree first:
        // byte 0 is the coefficient of x^(n−1)).
        let mut s = vec![0u8; self.p];
        for &b in block.iter() {
            for (si, t) in s.iter_mut().zip(&self.times) {
                *si = t[*si as usize] ^ b;
            }
        }
        if s.iter().all(|&x| x == 0) {
            return Ok(0);
        }
        // Berlekamp–Massey: error locator Λ(x), lowest degree first.
        let mut lambda = vec![0u8; self.p + 1];
        let mut b = vec![0u8; self.p + 1];
        lambda[0] = 1;
        b[0] = 1;
        let (mut l, mut m, mut bd) = (0usize, 1usize, 1u8);
        for r in 0..self.p {
            let mut d = s[r];
            for i in 1..=l {
                d ^= GF.mul(lambda[i], s[r - i]);
            }
            if d == 0 {
                m += 1;
                continue;
            }
            let coef = GF.div(d, bd);
            let prev = lambda.clone();
            for i in 0..=self.p - m {
                lambda[i + m] ^= GF.mul(coef, b[i]);
            }
            if 2 * l <= r {
                l = r + 1 - l;
                b = prev;
                bd = d;
                m = 1;
            } else {
                m += 1;
            }
        }
        if l > self.t() {
            return Err(RsUncorrectable);
        }
        // Error evaluator Ω(x) = S(x)·Λ(x) mod x^p.
        let mut omega = vec![0u8; self.p];
        for i in 0..self.p {
            for j in 0..=i.min(l) {
                omega[i] ^= GF.mul(lambda[j], s[i - j]);
            }
        }
        // Chien search over the n sent positions: byte j is degree n−1−j,
        // located by X = α^(n−1−j); Λ(X⁻¹) = 0 there.
        let mut fixed = 0;
        for (j, byte) in block.iter_mut().enumerate() {
            let deg = self.n - 1 - j;
            let xinv = GF.pow(255 - deg % 255);
            let mut v = 0u8;
            let mut xp = 1u8;
            for &c in &lambda[..=l] {
                v ^= GF.mul(c, xp);
                xp = GF.mul(xp, xinv);
            }
            if v != 0 {
                continue;
            }
            // Forney (first consecutive root 0): e = X·Ω(X⁻¹)/Λ'(X⁻¹).
            let mut num = 0u8;
            let mut xp = 1u8;
            for &c in &omega {
                num ^= GF.mul(c, xp);
                xp = GF.mul(xp, xinv);
            }
            let mut den = 0u8;
            let mut xp = 1u8; // X⁻¹ to the power i−1 for odd i
            for i in (1..=l).step_by(2) {
                den ^= GF.mul(lambda[i], xp);
                xp = GF.mul(xp, GF.mul(xinv, xinv));
            }
            if den == 0 {
                return Err(RsUncorrectable);
            }
            let x = GF.pow(deg);
            *byte ^= GF.mul(x, GF.div(num, den));
            fixed += 1;
        }
        if fixed != l {
            return Err(RsUncorrectable);
        }
        Ok(fixed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rand(seed: u64) -> impl FnMut() -> u64 {
        let mut s = seed | 1;
        move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    #[test]
    fn dvb_generator_matches_the_known_one() {
        // g(x) for RS(255,239) with roots α^0..α^15 over 0x11D; its x^0
        // coefficient is Π α^i = α^(0+1+…+15) = α^120.
        let rs = ReedSolomon::dvb();
        assert_eq!(rs.g.len(), 17);
        assert_eq!(rs.g[0], 1);
        assert_eq!(rs.g[16], GF.pow(120));
    }

    #[test]
    fn corrects_up_to_t_errors() {
        let mut next = rand(3);
        for (n, k) in [(204, 188), (126, 112), (219, 201), (225, 205), (194, 178)] {
            let rs = ReedSolomon::new(n, k);
            for errors in 0..=rs.t() {
                let mut block: Vec<u8> = (0..n).map(|_| next() as u8).collect();
                rs.encode(&mut block);
                let clean = block.clone();
                let mut hit = std::collections::BTreeSet::new();
                while hit.len() < errors {
                    hit.insert((next() % n as u64) as usize);
                }
                for &i in &hit {
                    block[i] ^= (next() as u8) | 1;
                }
                assert_eq!(rs.decode(&mut block), Ok(errors), "RS({n},{k}) {errors}");
                assert_eq!(block, clean);
            }
        }
    }

    #[test]
    fn too_many_errors_are_reported() {
        let mut next = rand(4);
        let rs = ReedSolomon::dvb();
        let mut flagged = 0;
        for _ in 0..50 {
            let mut block: Vec<u8> = (0..204).map(|_| next() as u8).collect();
            rs.encode(&mut block);
            for _ in 0..12 {
                let i = (next() % 204) as usize;
                block[i] ^= (next() as u8) | 1;
            }
            if rs.decode(&mut block).is_err() {
                flagged += 1;
            }
        }
        assert!(flagged >= 48, "{flagged} of 50");
    }
}
