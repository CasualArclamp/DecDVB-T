// made by CasualArclamp
//! What to transmit and where to send it. Plain data: building a
//! [`TxConfig`] never touches ffmpeg or the radio.

use std::path::PathBuf;

use decdvbt_core::dvbt::{Mode, Modulation, Rate};
use decdvbt_core::{Error, Result};

/// The IQ sample rate the radio is run at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TxRate {
    /// The modulator's own rate (64/7 MS/s for 8 MHz): no resampler, the
    /// cheapest. Whole hertz for the radio: 9 142 857 Hz, 0.016 ppm off.
    Native,
    /// A fixed rate, 2 to 20 MS/s (10 or 20 are exact ratios of native), by
    /// an interpolating polyphase filter. Costs CPU; moves the DAC images
    /// further from the channel.
    Fixed(u32),
}

impl TxRate {
    /// The rate in whole hertz for `mode`'s bandwidth.
    pub fn hz(self, mode: &Mode) -> u32 {
        match self {
            TxRate::Native => mode.bandwidth.sample_rate_hz().round() as u32,
            TxRate::Fixed(hz) => hz,
        }
    }
}

/// How the radio is driven.
#[derive(Debug, Clone, PartialEq)]
pub enum TxSink {
    /// Generate the signal and throw it away: tests and benchmarks. `paced`
    /// drains at the sample clock (a fake radio, to see underruns), else as
    /// fast as the modulator can go (the speed test).
    Null { paced: bool },
    /// Write cs8 IQ to a file (a name like `tx_546.000MHz_9.142857Msps.cs8`
    /// is what the receiver reads the tuning from). `paced` as for `Null`;
    /// `seconds` ends the session after that much signal.
    IqFile {
        path: PathBuf,
        paced: bool,
        seconds: Option<f64>,
    },
    /// The HackRF One. Reachable only through [`crate::arm::Arm`].
    HackRf(HackRfTxConfig),
}

impl TxSink {
    /// Whether this sink radiates.
    pub fn is_radio(&self) -> bool {
        matches!(self, TxSink::HackRf(_))
    }
}

/// The HackRF's transmit settings. The defaults are the safe ones.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HackRfTxConfig {
    /// TX VGA, 0 to 47 dB. Starts at 0.
    pub txvga_db: u16,
    /// The +14 dB RF amplifier. Starts off.
    pub amp: bool,
    /// The radio's baseband filter, Hz; `None` leaves the driver's automatic
    /// choice (75 % of the rate, which cuts the edges of a 7.61 MHz signal at
    /// 9.14 MS/s). The default is 8 MHz.
    pub baseband_filter_hz: Option<u32>,
}

impl Default for HackRfTxConfig {
    fn default() -> Self {
        HackRfTxConfig {
            txvga_db: 0,
            amp: false,
            baseband_filter_hz: Some(8_000_000),
        }
    }
}

/// Which encoder chain makes the video.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncoderChoice {
    /// NVENC with CUDA decode and scale if a test encode works, else x264.
    #[default]
    Auto,
    /// The NVIDIA chain only (fails if it does not work).
    Nvenc,
    /// libx264 `veryfast`, hardware decode where it helps. Works anywhere.
    X264,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoCodec {
    #[default]
    H264,
    /// MPEG-2 video: what the oldest receivers want (pairs with MP2 audio).
    Mpeg2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioCodec {
    /// AAC-LC in LATM (PMT stream type 0x11), what UK receivers expect.
    #[default]
    AacLatm,
    /// AAC-LC in ADTS (stream type 0x0F).
    AacAdts,
    /// MPEG-1 layer II: the safest for hardware receivers.
    Mp2,
}

/// What ffmpeg is asked for. Sizes and bitrates come from the mode
/// ([`crate::modes::VideoProfile::for_rate`]) unless overridden.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoSettings {
    pub encoder: EncoderChoice,
    pub video_codec: VideoCodec,
    pub audio_codec: AudioCodec,
    /// Output frame rate (UK: 25).
    pub fps: u32,
    /// Output picture size; `None` picks 720x576, 1280x720 or 1920x1080 by
    /// the mode's bitrate.
    pub size: Option<(u32, u32)>,
    /// Keyframe interval, seconds (receivers join on one): 1.
    pub gop_seconds: u32,
}

impl Default for VideoSettings {
    fn default() -> Self {
        VideoSettings {
            encoder: EncoderChoice::Auto,
            video_codec: VideoCodec::H264,
            audio_codec: AudioCodec::AacLatm,
            fps: 25,
            size: None,
            gop_seconds: 1,
        }
    }
}

/// The service we announce. The defaults use the "private use" id ranges
/// (original_network_id 0xFF00 to 0xFFFF) so nothing here can be mistaken for
/// a real broadcaster's mux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    pub service_id: u16,
    pub original_network_id: u16,
    pub network_id: u16,
    pub transport_stream_id: u16,
    pub provider: String,
    /// Shown by the receiver; the GUI defaults it to the file's name.
    pub name: String,
    pub network_name: String,
}

impl Default for ServiceInfo {
    fn default() -> Self {
        ServiceInfo {
            service_id: 1,
            original_network_id: 0xFF01,
            network_id: 0xFF01,
            transport_stream_id: 1,
            provider: "DecDVB-T".into(),
            name: "DecDVB-T TX".into(),
            network_name: "DecDVB-T".into(),
        }
    }
}

/// Everything a [`crate::session::TxSession`] needs.
#[derive(Debug, Clone, PartialEq)]
pub struct TxConfig {
    /// The video file (anything ffmpeg reads).
    pub input: PathBuf,
    /// Start again at the end of the file (inside ffmpeg, so counters and PCR
    /// stay continuous). Off: the session finishes when the file does.
    pub loop_input: bool,
    /// The DVB-T mode. Non-hierarchical only.
    pub mode: Mode,
    /// RF centre frequency, Hz.
    pub centre_hz: f64,
    pub rate: TxRate,
    /// The signal's rms level in dBFS (of |c|), before the radio's gain.
    /// -12 keeps 37.9 dB of 8-bit quantisation SNR with no clipping.
    pub level_dbfs: f32,
    pub video: VideoSettings,
    pub service: ServiceInfo,
    pub sink: TxSink,
    /// Where ffmpeg is, if the user said; else it is looked for.
    pub ffmpeg_path: Option<PathBuf>,
    /// Null packets sent before the programme starts, so a receiver can lock
    /// first. Milliseconds of signal.
    pub lead_in_ms: u32,
    /// Compute spectrum rows of the signal for the GUI.
    pub preview: bool,
}

impl TxConfig {
    /// A sensible start: the UK mode (64-QAM 2/3 8k 1/32), C30's centre,
    /// native rate, -12 dBFS, and the `Null` sink (never the radio).
    pub fn new(input: PathBuf) -> TxConfig {
        TxConfig {
            input,
            loop_input: true,
            mode: Mode::uk(Modulation::Qam64, Rate::R2_3),
            centre_hz: 546_000_000.0,
            rate: TxRate::Native,
            level_dbfs: -12.0,
            video: VideoSettings::default(),
            service: ServiceInfo::default(),
            sink: TxSink::Null { paced: true },
            ffmpeg_path: None,
            lead_in_ms: 600,
            preview: true,
        }
    }

    /// The radio rate, whole hertz.
    pub fn sample_rate_hz(&self) -> u32 {
        self.rate.hz(&self.mode)
    }

    /// Check the settings against what the modulator and the radio accept.
    pub fn validate(&self) -> Result<()> {
        if self.mode.hierarchical() {
            return Err(Error::Other("hierarchical modes are not supported".into()));
        }
        let hz = self.sample_rate_hz();
        if !(2_000_000..=20_000_000).contains(&hz) {
            return Err(Error::Other(format!(
                "sample rate {hz} Hz is outside the HackRF's 2 to 20 MS/s"
            )));
        }
        if (hz as f64) < self.mode.bandwidth.sample_rate_hz() * 0.999 {
            return Err(Error::Other(
                "sample rate is below the signal's own (it would alias)".into(),
            ));
        }
        if !(1_000_000.0..=6_000_000_000.0).contains(&self.centre_hz) {
            return Err(Error::Other("centre is outside 1 MHz to 6 GHz".into()));
        }
        if !(-30.0..=-6.0).contains(&self.level_dbfs) {
            return Err(Error::Other("level must be -30 to -6 dBFS".into()));
        }
        if self.video.fps == 0 || self.video.fps > 60 {
            return Err(Error::Other("frame rate must be 1 to 60".into()));
        }
        if self.input.as_os_str().is_empty() {
            return Err(Error::Other("no input file".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_config_never_reaches_the_radio() {
        let c = TxConfig::new("x.mp4".into());
        assert!(!c.sink.is_radio());
        assert!(c.validate().is_ok());
        assert_eq!(c.sample_rate_hz(), 9_142_857);
    }

    #[test]
    fn hackrf_defaults_are_the_safe_ones() {
        let h = HackRfTxConfig::default();
        assert_eq!((h.txvga_db, h.amp), (0, false));
    }

    #[test]
    fn rejects_a_rate_the_signal_would_alias_in() {
        let mut c = TxConfig::new("x.mp4".into());
        c.rate = TxRate::Fixed(8_000_000);
        assert!(c.validate().is_err());
    }
}
