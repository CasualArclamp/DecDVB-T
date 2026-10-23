// made by CasualArclamp
//! A DVB-T modulator and channel models, for test signals: the receiver can
//! be built against signals whose every bit is known, and put through
//! multipath, Doppler, noise and the radio's own faults.
//!
//! - [`ts`]: transport streams to send: a test stream the receiver can check
//!   packet by packet, or a `.ts` file.
//! - [`outer`]: energy dispersal, Reed–Solomon, the byte interleaver.
//! - [`inner`]: the convolutional code, the bit and symbol interleavers and
//!   the mapper.
//! - [`modulator`]: pilots, TPS, the transform and the guard interval.
//! - [`channel`]: multipath, fading, Doppler, noise and the radio's faults.
//! - [`rng`]: the seeded generator the channel uses.
//! - [`sim`]: a whole test signal: modulator, channel, resampler, level.
//! - [`t2`]: the DVB-T2 modulator.
//!
//! Hierarchical modulation is not supported yet.

pub mod channel;
pub mod inner;
pub mod modulator;
pub mod outer;
pub mod rng;
pub mod sim;
pub mod t2;
pub mod ts;

pub use channel::{Channel, ChannelSim, RadioSim};
pub use modulator::Modulator;
