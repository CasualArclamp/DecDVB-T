// made by CasualArclamp
//! The DVB-T receiver's front end and channel sounder.
//!
//! - [`acquire`]: find the mode, timing, carrier offset and pilot phase.
//! - [`lock`]: lock on to a signal in a stretch of samples.
//! - [`interp`]: a sinc interpolator, reading the radio's samples at the
//!   signal's rate and at fractional positions.
//! - [`delay_doppler`]: the delay–Doppler map from the channel estimates.
//! - [`sounder`]: acquisition, tracking, and the channel estimate rows.
//! - [`symbol`]: reading OFDM symbols, with the timing and carrier tracking.
//! - [`t2`]: the DVB-T2 receiver.
//! - [`auto`]: DVB-T or DVB-T2, whichever the signal is.

pub mod acquire;
pub mod auto;
pub mod chest;
pub mod delay_doppler;
pub mod demap;
pub mod interp;
pub mod lock;
pub mod outer;
pub mod receiver;
pub mod scope;
pub mod sounder;
pub mod symbol;
pub mod t2;
pub mod tps_rx;
pub mod window;
