// made by CasualArclamp
//! DSP building blocks for DecDVB-T.
//!
//! - [`ddc`]: the digital down-converter: NCO mix and decimation to the
//!   channel's rate.
//! - [`fir`] / [`dot`]: a streaming complex FIR and its SIMD dot product.
//! - [`agc`]: block AGC to unit power.
//! - [`dc`]: DC removal.

pub mod agc;
pub mod dc;
pub mod ddc;
pub mod dot;
pub mod fir;

pub use agc::Agc;
pub use dc::DcBlocker;
pub use ddc::{Ddc, lowpass};
pub use fir::Fir;
