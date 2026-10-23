// made by CasualArclamp
//! In-app sound: PCM decoded by ffmpeg (the TV player) to the sound card.
//!
//! - [`player`]: the thread and sound output, with volume, mute, pause and
//!   the output device chosen by name.
//! - [`resample`]: to the sound card's rate, drift-trimmed.

pub mod player;
pub mod resample;

pub use player::{
    AudioHandle, AudioPlayer, OutputKind, PlayState, Status, muted, output_changes, output_device,
    output_devices, set_muted, set_output_device, set_volume, volume,
};
