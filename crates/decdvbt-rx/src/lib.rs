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

pub mod acquire;
pub mod chest;
pub mod delay_doppler;
pub mod demap;
pub mod interp;
pub mod lock;
pub mod outer;
pub mod receiver;
pub mod sounder;
pub mod symbol;
pub mod tps_rx;
pub mod window;
