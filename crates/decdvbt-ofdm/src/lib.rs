// made by CasualArclamp
//! DVB-T OFDM definitions shared by the modulator (`decdvbt-mod`) and the
//! receiver (M3): what each carrier of a symbol is and carries.
//!
//! - [`pilots`]: continual, scattered and TPS carrier positions, the
//!   reference PRBS and pilot values, and the data carriers of each symbol.
//! - [`tps`]: the 68-bit TPS block, its BCH code, and reading it back.
//! - [`interleave`]: the inner interleavers: bits, then symbol cells.
//! - [`mapping`]: the Gray-coded constellations.

pub mod interleave;
pub mod mapping;
pub mod pilots;
pub mod tps;
