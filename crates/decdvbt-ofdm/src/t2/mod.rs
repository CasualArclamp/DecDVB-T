// made by CasualArclamp
//! DVB-T2 OFDM definitions shared by the modulator and the receiver (EN 302
//! 755): which carrier of which symbol is a pilot, a reserved carrier or a
//! data cell, the values the pilots carry, the frequency interleaver and the
//! P1 preamble.
//!
//! The tables come from gr-dtv's DVB-T2 modulator (GPL-3.0-or-later,
//! Copyright 2015-2019 Free Software Foundation, Inc.; after Ron Economos's
//! DVB-T2 modulator), which has been checked against real receivers, and are
//! checked here against its cell counts and, for the UK's 32K extended-mode
//! PP7 multiplex, against a real capture.
//!
//! - [`params`]: FFT size, guard interval, pilot pattern and the carrier
//!   counts that follow from them.
//! - [`carriers`]: the role of every carrier in the P2, data and frame-closing
//!   symbols, and the pilots' values.
//! - [`freq_il`]: the frequency interleaver's permutations.
//! - [`l1`]: the L1 signalling's fields.
//! - [`p1`]: the P1 symbol: its signalling sequences, its samples.

pub mod carriers;
pub mod freq_il;
pub mod l1;
pub mod p1;
pub mod params;
pub mod tables;

pub use carriers::{Carriers, Reference, Role};
pub use freq_il::FreqInterleaver;
pub use l1::{L1Post, L1Pre};
pub use params::{Fft, Guard, Mode, PilotPattern};
