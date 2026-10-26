// made by CasualArclamp
//! The DecDVB-T transmit engine: a video file in, a DVB-T signal out through
//! a HackRF One (or into an IQ file, or nowhere, for tests).
//!
//! Read `docs/TX_DESIGN.md` first; it is the contract between the builders.
//! The short version of the data flow:
//!
//! ```text
//! video file --ffmpeg (child)--> pipe --[feeder]--> TsRing --[tx-mod]--> IqRing --[radio]--> HackRF
//!                                                   (nulls stuffed        (zeros sent
//!                                                    if it is empty)       if it is empty)
//! ```
//!
//! The radio's sample clock paces everything: its USB drain empties the IQ
//! ring, which lets the modulator thread run, which pulls packets from the TS
//! ring, which lets ffmpeg's pipe drain. Nothing runs on a clock of its own.
//!
//! **Nothing here can reach the radio without [`arm::Arm`].** The one call that
//! opens the HackRF is in [`radio`], takes an [`arm::ArmedProof`] that only
//! [`arm`] can make, and a test checks that no other file mentions it.
//!
//! - [`config`]: what to send ([`TxConfig`]) and where to ([`TxSink`]).
//! - [`modes`]: the DVB-T modes with their bitrates, and the video profile
//!   (size, bitrates, audio) that fits each.
//! - [`ffmpeg`]: finding ffmpeg and ffprobe, probing the file, the command lines.
//! - [`feeder`]: the ffmpeg child process and the threads that read it.
//! - [`ring`]: the two bounded FIFOs.
//! - [`si`]: the NIT, TDT and TOT we put into ffmpeg's stream.
//! - [`ts_source`]: the `TsSource` the modulator pulls from.
//! - [`iq`]: modulator, level, resampler and cs8 packing, as a pull source.
//! - [`preview`]: spectrum rows of what is being sent.
//! - [`arm`]: the arm gate.
//! - [`radio`]: the sinks (HackRF, IQ file, null) behind one pull contract.
//! - [`session`]: [`TxSession`], what the GUI drives, and [`TxStatus`].
//! - [`cpu`]: the process CPU meter.

pub mod arm;
pub mod config;
pub mod cpu;
pub mod feeder;
pub mod ffmpeg;
pub mod iq;
pub mod modes;
pub mod preview;
pub mod radio;
pub mod ring;
pub mod session;
pub mod si;
pub mod ts_source;

pub use arm::{Arm, ArmedProof, LICENCE_WARNING, LicenceAck};
pub use config::{
    AudioCodec, EncoderChoice, HackRfTxConfig, ServiceInfo, TxConfig, TxRate, TxSink, VideoCodec,
    VideoSettings,
};
pub use modes::{ModeRow, VideoProfile, mode_table, mux_rate_bps};
pub use preview::PreviewRow;
pub use session::{TxSession, TxState, TxStatus};

pub use decdvbt_core::{Error, Result};
