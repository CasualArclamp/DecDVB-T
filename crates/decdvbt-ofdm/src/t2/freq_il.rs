// made by CasualArclamp
//! The frequency interleaver (EN 302 755 §8.3.4): the cells of each symbol
//! are permuted over the symbol's carriers with a pseudo-random permutation
//! that is different in even and odd symbols.
//!
//! The permutation H(q) is the register sequence of a PRBS of 2^(degree+1)
//! states, its bits permuted, and a toggling top bit, with the entries that
//! are not below the cell count dropped. As in gr-dtv's
//! `dvbt2_freqinterleaver_cc_impl.cc` (GPL-3.0-or-later, Copyright 2015-2019
//! Free Software Foundation, Inc.), including its handling of 32K, where the
//! two symbol parities share one bit permutation and the even one is the
//! inverse.

use super::params::Fft;
use super::tables::*;

/// (register degree, feedback taps, even bit permutation, odd bit
/// permutation) of an FFT size.
fn generator(fft: Fft) -> (u32, &'static [u32], &'static [u8], &'static [u8]) {
    match fft {
        Fft::K1 => (9, &[0, 4], &BITPERM1KEVEN, &BITPERM1KODD),
        Fft::K2 => (10, &[0, 3], &BITPERM2KEVEN, &BITPERM2KODD),
        Fft::K4 => (11, &[0, 2], &BITPERM4KEVEN, &BITPERM4KODD),
        Fft::K8 => (12, &[0, 1, 4, 6], &BITPERM8KEVEN, &BITPERM8KODD),
        Fft::K16 => (13, &[0, 1, 4, 5, 9, 11], &BITPERM16KEVEN, &BITPERM16KODD),
        Fft::K32 => (14, &[0, 1, 2, 12], &BITPERM32K, &BITPERM32K),
    }
}

/// The permutations of one kind of symbol (P2, data or frame closing).
pub struct FreqInterleaver {
    even: Vec<u16>,
    odd: Vec<u16>,
}

impl FreqInterleaver {
    /// The permutations over `cells` cells (fewer than 2^(degree+1)).
    pub fn new(fft: Fft, cells: usize) -> FreqInterleaver {
        let (degree, taps, perm_even, perm_odd) = generator(fft);
        let states = 1usize << (degree + 1);
        assert!(cells <= states);
        let mask = (1u32 << degree) - 1;
        let (mut even, mut odd) = (Vec::with_capacity(cells), Vec::with_capacity(cells));
        let mut lfsr: u32 = 0;
        for i in 0..states {
            if i < 2 {
                lfsr = 0;
            } else if i == 2 {
                lfsr = 1;
            } else {
                let result = taps.iter().fold(0, |a, &t| a ^ ((lfsr >> t) & 1));
                lfsr &= mask;
                lfsr >>= 1;
                lfsr |= result << (degree - 1);
            }
            let permute = |perm: &[u8]| -> usize {
                (0..degree as usize)
                    .map(|n| (((lfsr >> n) & 1) as usize) << perm[n])
                    .sum::<usize>()
                    + (i % 2) * (states / 2)
            };
            let (e, o) = (permute(perm_even), permute(perm_odd));
            if e < cells {
                even.push(e as u16);
            }
            if o < cells {
                odd.push(o as u16);
            }
        }
        debug_assert_eq!(even.len(), cells);
        debug_assert_eq!(odd.len(), cells);
        if fft == Fft::K32 {
            // One permutation, used forwards in odd symbols and inverted in
            // even ones.
            for (j, &a) in odd.iter().enumerate() {
                even[a as usize] = j as u16;
            }
        }
        FreqInterleaver { even, odd }
    }

    /// The permutation of the `symbol`-th symbol of a frame (counted from the
    /// first P2 symbol as 0): output cell j is input cell H[j].
    pub fn h(&self, symbol: usize) -> &[u16] {
        if symbol.is_multiple_of(2) {
            &self.even
        } else {
            &self.odd
        }
    }

    pub fn cells(&self) -> usize {
        self.even.len()
    }

    /// Interleave one symbol's cells (the transmitter's).
    pub fn interleave<T: Copy>(&self, symbol: usize, input: &[T], out: &mut [T]) {
        for (o, &h) in out.iter_mut().zip(self.h(symbol)) {
            *o = input[h as usize];
        }
    }

    /// Undo it (the receiver's): input cell j goes back to place H[j].
    pub fn deinterleave<T: Copy>(&self, symbol: usize, input: &[T], out: &mut [T]) {
        for (&v, &h) in input.iter().zip(self.h(symbol)) {
            out[h as usize] = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_permutation_is_a_permutation() {
        for fft in Fft::ALL {
            let cells = match fft {
                Fft::K1 => 764,
                Fft::K2 => 1522,
                Fft::K4 => 3084,
                Fft::K8 => 6208,
                Fft::K16 => 12418,
                Fft::K32 => 26022,
            };
            let fi = FreqInterleaver::new(fft, cells);
            for sym in 0..2 {
                let mut seen = vec![false; cells];
                for &h in fi.h(sym) {
                    assert!(!seen[h as usize], "{fft} symbol {sym}: {h} twice");
                    seen[h as usize] = true;
                }
            }
            // Even and odd differ.
            assert_ne!(fi.h(0), fi.h(1), "{fft}");
        }
    }

    #[test]
    fn deinterleaving_undoes_interleaving() {
        let fi = FreqInterleaver::new(Fft::K32, 27404);
        let input: Vec<u32> = (0..27404).collect();
        let (mut a, mut b) = (vec![0u32; 27404], vec![0u32; 27404]);
        for sym in 0..3 {
            fi.interleave(sym, &input, &mut a);
            fi.deinterleave(sym, &a, &mut b);
            assert_eq!(b, input);
        }
    }

    #[test]
    fn the_first_entries_of_the_1k_permutation() {
        // From the register sequence: state 0 and 1 are 0, so H(0) = 0 and
        // H(1) = 512 (the toggling top bit): the 1K permutation starts there.
        let fi = FreqInterleaver::new(Fft::K1, 764);
        assert_eq!(fi.h(0)[0], 0);
        assert_eq!(fi.h(0)[1], 512);
        assert_eq!(fi.h(1)[0], 0);
    }

    #[test]
    fn in_32k_the_two_parities_are_inverses() {
        let fi = FreqInterleaver::new(Fft::K32, 17612);
        for q in 0..17612usize {
            assert_eq!(fi.h(0)[fi.h(1)[q] as usize] as usize, q);
        }
    }
}
