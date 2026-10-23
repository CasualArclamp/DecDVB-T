// made by CasualArclamp
//! Forward error correction for DVB-T and DVB-T2.
//!
//! DVB-T (EN 300 744 §4.3):
//! - [`conv`]: the K = 7 inner convolutional code, punctured rates and a
//!   soft-decision Viterbi decoder.
//! - [`rs`]: the outer RS(204,188, t = 8) code.
//!
//! DVB-T2 uses DVB-S2's LDPC and BCH codes (EN 302 755 §6.1), so these come
//! from DecSAT as they were; the S2/S2X constellations and tables go or move
//! to T2's own when the T2 receiver lands (M6):
//! - [`params`]: code sizes per frame length and rate (Tables 5a/5b).
//! - [`bch`]: the outer BCH code.
//! - [`ldpc`]: the inner LDPC code, encoder and layered min-sum decoder.
//! - [`constellation`]: the S2 constellations and their bit labels.
//! - [`demap`]: bit interleaving, mapping, and max-log soft demapping.
//!
//! - [`apsk_tables`]: the S2X constellation and interleaver tables.
//! - [`vlsnr`]: the shortened and punctured codes of S2X VL-SNR frames.

pub mod apsk_tables;
pub mod bch;
pub mod constellation;
pub mod conv;
pub mod demap;
pub mod ldpc;
pub mod params;
pub mod rs;
pub mod vlsnr;

pub use bch::{Bch, BchError};
pub use constellation::Constellation;
pub use ldpc::{DecodeOutcome, LdpcCode, LdpcDecoder};
pub use params::FecParams;
