// made by CasualArclamp
//! The DecDVB-T engine.
//!
//! - [`rx`]: the receive thread: source in, waterfall rows, levels and a
//!   recording out.
//! - [`hub`]: the transport stream after the receiver: analyser, outputs, the
//!   player's tap.
//! - [`spectrum`]: the averaged spectrum rows, built a few FFTs a block.
//! - [`meter`]: CPU meters, whole-process and per stage.
//! - [`snr`]: a C/N estimate from the spectrum, until the receiver measures MER.

pub mod frontend;
pub mod hub;
pub mod meter;
pub mod rx;
pub mod snr;
pub mod spectrum;

pub use frontend::{FrontEndReading, FrontEndSettings};
pub use hub::{CarouselLink, CarouselState, Outputs, TsHub, TsView};
pub use meter::{CpuGauge, Stage, StageLoad, StageMeter, process_cpu_seconds};
pub use rx::{
    Command, DecoderUpdate, DropProbe, Recording, Row, Rx, RxOptions, SounderUpdate, Stats,
};
pub use snr::{signal_block, spectral_cn_db};
pub use spectrum::{RowBuilder, Spectrum};
