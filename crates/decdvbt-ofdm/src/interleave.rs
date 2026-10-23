// made by CasualArclamp
//! The inner interleaver (EN 300 744 §4.3.4): a bit interleaver, in blocks
//! of 126 cells over the v bits of a cell, then a symbol interleaver
//! permuting the data cells of each OFDM symbol.
//!
//! Tables and the register for the symbol permutation are gr-dtv's
//! (`dvbt_symbol_inner_interleaver_impl.cc`, `dvbt_bit_inner_interleaver_impl.cc`;
//! GPL-3, Free Software Foundation), with the spec's structure.

use decdvbt_core::dvbt::{FftSize, Modulation};

/// Cells in a bit-interleaver block (§4.3.4.1).
pub const BLOCK: usize = 126;

/// The offsets of the bit interleavers' permutations H_e(w) = (w + offset)
/// mod 126, e = 0 … 5 (§4.3.4.1).
const OFFSETS: [usize; 6] = [0, 63, 105, 42, 21, 84];

/// Which of a cell's v bits go to which of the v bit interleavers, in order:
/// the bit word y0…y(v−1) is demultiplexed to b0, b2, b4, b1, b3, b5
/// (64-QAM), b0, b2, b1, b3 (16-QAM), b0, b1 (QPSK): odd bits of the word
/// feed the second half (§4.3.4.1).
pub fn demux_order(m: Modulation) -> &'static [usize] {
    match m {
        Modulation::Qpsk => &[0, 1],
        Modulation::Qam16 => &[0, 2, 1, 3],
        Modulation::Qam64 => &[0, 2, 4, 1, 3, 5],
    }
}

/// Bit-interleave one block: `words` are 126 cells' bit words (v bits, y0
/// the most significant), returned interleaved. Non-hierarchical.
pub fn bit_interleave(m: Modulation, words: &[u8], out: &mut [u8]) {
    assert_eq!(words.len(), BLOCK);
    assert_eq!(out.len(), BLOCK);
    let v = m.bits();
    let order = demux_order(m);
    // b[e][i]: the bit that interleaver e holds at position i.
    let mut b = [[0u8; BLOCK]; 6];
    for (i, &word) in words.iter().enumerate() {
        for (k, &e) in order.iter().enumerate() {
            b[e][i] = (word >> (v - 1 - k)) & 1;
        }
    }
    for (w, o) in out.iter_mut().enumerate() {
        let mut word = 0u8;
        for (e, &off) in OFFSETS.iter().enumerate().take(v) {
            word = (word << 1) | b[e][(w + off) % BLOCK];
        }
        *o = word;
    }
}

/// The inverse of [`bit_interleave`].
pub fn bit_deinterleave(m: Modulation, words: &[u8], out: &mut [u8]) {
    assert_eq!(words.len(), BLOCK);
    assert_eq!(out.len(), BLOCK);
    let v = m.bits();
    let order = demux_order(m);
    let mut b = [[0u8; BLOCK]; 6];
    for (w, &word) in words.iter().enumerate() {
        for (e, &off) in OFFSETS.iter().enumerate().take(v) {
            b[e][(w + off) % BLOCK] = (word >> (v - 1 - e)) & 1;
        }
    }
    for (i, o) in out.iter_mut().enumerate() {
        let mut word = 0u8;
        for (k, &e) in order.iter().enumerate() {
            word |= b[e][i] << (v - 1 - k);
        }
        *o = word;
    }
}

/// The symbol interleaver's permutation H(q), q = 0 … Nmax−1 (§4.3.4.2): a
/// register of Nr bits run once for each of the Mmax = 2^Nr positions,
/// its bits permuted, a parity bit put on top, and the values that fit in
/// Nmax kept.
pub fn symbol_permutation(fft: FftSize) -> Vec<u16> {
    let (nr, bit_perm): (u32, &[u32]) = match fft {
        FftSize::K2 => (11, &[4, 3, 9, 6, 2, 8, 1, 5, 7, 0]),
        FftSize::K8 => (13, &[7, 1, 4, 2, 9, 6, 8, 10, 0, 3, 11, 5]),
    };
    let mmax = 1usize << nr;
    let nmax = fft.data_carriers();
    let mask = (1u32 << nr) - 1;
    let mut h = Vec::with_capacity(nmax);
    // R'_i: 0 for i = 0, 1; 1 for i = 2; then a step of the register.
    let mut reg: u32 = 0;
    for i in 0..mmax {
        match i {
            0 | 1 => reg = 0,
            2 => reg = 1,
            _ => {
                let new_bit = match fft {
                    FftSize::K2 => (reg ^ (reg >> 3)) & 1,
                    FftSize::K8 => (reg ^ (reg >> 1) ^ (reg >> 4) ^ (reg >> 6)) & 1,
                };
                reg = ((reg >> 1) | (new_bit << (nr - 2))) & mask;
            }
        }
        // Permute the register's Nr − 1 bits.
        let mut r = 0u32;
        for (k, &to) in bit_perm.iter().enumerate() {
            r |= ((reg >> k) & 1) << to;
        }
        let q = ((i as u32 % 2) << (nr - 1)) + r;
        if (q as usize) < nmax {
            h.push(q as u16);
        }
    }
    h
}

/// Interleave one OFDM symbol's data cells: even symbols (counted in the
/// frame) y[H(q)] = x[q], odd y[q] = x[H(q)] (§4.3.4.2).
pub fn symbol_interleave<T: Copy>(h: &[u16], l: usize, x: &[T], y: &mut [T]) {
    assert_eq!(x.len(), h.len());
    assert_eq!(y.len(), h.len());
    if l.is_multiple_of(2) {
        for (q, &hq) in h.iter().enumerate() {
            y[hq as usize] = x[q];
        }
    } else {
        for (q, &hq) in h.iter().enumerate() {
            y[q] = x[hq as usize];
        }
    }
}

/// The inverse of [`symbol_interleave`].
pub fn symbol_deinterleave<T: Copy>(h: &[u16], l: usize, y: &[T], x: &mut [T]) {
    assert_eq!(x.len(), h.len());
    assert_eq!(y.len(), h.len());
    if l.is_multiple_of(2) {
        for (q, &hq) in h.iter().enumerate() {
            x[q] = y[hq as usize];
        }
    } else {
        for (q, &hq) in h.iter().enumerate() {
            x[hq as usize] = y[q];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_symbol_permutation_is_a_permutation() {
        // Only a correct register and bit permutation gives each of the
        // 1512 / 6048 positions exactly once.
        for fft in [FftSize::K2, FftSize::K8] {
            let h = symbol_permutation(fft);
            assert_eq!(h.len(), fft.data_carriers());
            let mut seen = vec![false; h.len()];
            for &v in &h {
                assert!((v as usize) < h.len());
                assert!(!seen[v as usize], "{fft:?}: {v} twice");
                seen[v as usize] = true;
            }
        }
    }

    #[test]
    fn the_permutation_starts_as_the_spec_gives_it() {
        // i = 0: R = 0, parity 0 → 0. i = 1: R = 0, odd → the parity bit
        // alone, 2^(Nr−1). i = 2: R'_2 has one bit set, which the bit
        // permutation sends to position 4 (2k) or 7 (8k).
        let h = symbol_permutation(FftSize::K2);
        assert_eq!(&h[..3], &[0, 1024, 16]);
        let h = symbol_permutation(FftSize::K8);
        assert_eq!(&h[..3], &[0, 4096, 128]);
    }

    #[test]
    fn symbol_interleaving_round_trips_on_even_and_odd_symbols() {
        let h = symbol_permutation(FftSize::K2);
        let x: Vec<u32> = (0..h.len() as u32).collect();
        for l in [0usize, 1, 2, 3, 67] {
            let mut y = vec![0; x.len()];
            let mut back = vec![0; x.len()];
            symbol_interleave(&h, l, &x, &mut y);
            assert_ne!(x, y);
            symbol_deinterleave(&h, l, &y, &mut back);
            assert_eq!(x, back, "symbol {l}");
        }
        // Even and odd symbols are inverse permutations of each other.
        let mut a = vec![0; x.len()];
        let mut b = vec![0; x.len()];
        symbol_interleave(&h, 0, &x, &mut a);
        symbol_interleave(&h, 1, &a, &mut b);
        assert_eq!(x, b);
    }

    #[test]
    fn bit_interleaving_round_trips() {
        for m in [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64] {
            let v = m.bits();
            let words: Vec<u8> = (0..BLOCK)
                .map(|i| ((i * 37 + 11) % (1 << v)) as u8)
                .collect();
            let mut y = vec![0u8; BLOCK];
            let mut back = vec![0u8; BLOCK];
            bit_interleave(m, &words, &mut y);
            assert_ne!(words, y, "{m:?}");
            bit_deinterleave(m, &y, &mut back);
            assert_eq!(words, back, "{m:?}");
        }
    }

    #[test]
    fn bit_interleaving_moves_every_bit_once() {
        // The same number of ones in and out, whatever the order.
        let m = Modulation::Qam64;
        let words: Vec<u8> = (0..BLOCK).map(|i| (i % 64) as u8).collect();
        let mut y = vec![0u8; BLOCK];
        bit_interleave(m, &words, &mut y);
        let ones = |w: &[u8]| w.iter().map(|b| b.count_ones()).sum::<u32>();
        assert_eq!(ones(&words), ones(&y));
    }
}
