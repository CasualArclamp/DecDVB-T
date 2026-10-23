// made by CasualArclamp
//! The DVB-T receiver's front end and channel sounder.
//!
//! - [`acquire`]: find the mode, timing, carrier offset and pilot phase.
//! - [`interp`]: a sinc interpolator, reading the radio's samples at the
//!   signal's rate and at fractional positions.
//! - [`delay_doppler`]: the delay–Doppler map from the channel estimates.
//! - [`sounder`]: acquisition, tracking, and the channel estimate rows.

pub mod acquire;
pub mod delay_doppler;
pub mod interp;
pub mod sounder;
