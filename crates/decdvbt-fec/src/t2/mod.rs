// made by CasualArclamp
//! The DVB-T2 physical layer's coding, beyond the LDPC and BCH codes it shares
//! with DVB-S2 (EN 302 755 §5-7):
//!
//! - [`qam`]: the QPSK, 16-, 64- and 256-QAM constellations with their
//!   rotation, and soft demapping (rotated or not).
//! - [`bbframe`]: the BBFRAME's header and its transport packets.
//! - [`bitmap`]: the bit interleaver and the demultiplexer that turn a
//!   FECFRAME into the labels of its cells, and back.
//! - [`l1`]: the coding of the L1 signalling (BCH, LDPC, puncturing, padding).
//! - [`timeint`]: the cell interleaver and the time interleaver.
//! - [`plp`]: a physical layer pipe's FEC chain: BBFRAME scrambling, BCH,
//!   LDPC, interleaving, mapping, and the way back.
//!
//! The tables and the constructions are those of gr-dtv's DVB-T2 blocks
//! (GPL-3.0-or-later, Copyright 2015-2019 Free Software Foundation, Inc.).

pub mod bbframe;
pub mod bitmap;
pub mod l1;
pub mod plp;
pub mod qam;
pub mod timeint;

pub use bitmap::BitMap;
pub use qam::{Constellation, Qam};
