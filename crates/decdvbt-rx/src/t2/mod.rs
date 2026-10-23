// made by CasualArclamp
//! The DVB-T2 receiver: a T2 signal in the radio's samples to a transport
//! stream. It works a frame at a time (a frame is about 200 ms, P1 symbol to
//! the next), because DVB-T2's time interleaver spans one, and a frame gives
//! the timing and the carrier offset many symbols to be measured over.
//!
//! - [`frame`]: a frame's symbols to equalised cells.
//! - [`plp`]: a PLP's cells of a frame to transport packets.
//! - [`pipe`]: the FEC stage on a thread of its own.
//! - [`receiver`]: acquisition, tracking, and the whole receive chain.
//! - [`p1det`]: finding P1 symbols, reading S1 and S2 and the carrier offset.

pub mod frame;
pub mod p1det;
pub mod pipe;
pub mod plp;
pub mod receiver;
