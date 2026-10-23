// made by CasualArclamp
//! IQ file replay and recording.
//!
//! Formats are the ones the SDR world actually hands around: `cs8` (the
//! HackRF's native signed 8-bit interleaved), `cu8` (the RTL-SDR's), `cs16`
//! and `cf32` — and two-channel WAV (SDR#, HDSDR, SDRuno, SDR++), whose
//! header gives the format and rate (see [`crate::wav`]).

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use decdvbt_core::{Iq, Result, SampleFormat, bytes_to_iq};

use crate::IqSource;

/// Guess the sample format from a file extension, following the common
/// conventions (`.cs8`/`.s8`/`.iq8`, `.cs16`/`.s16`, `.cf32`/`.fc32`).
///
/// Returns `None` when the extension says nothing; the caller should then ask
/// the user rather than guess, since reading cs16 as cs8 silently "works".
pub fn format_from_path(path: &Path) -> Option<SampleFormat> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "cs8" | "s8" | "iq8" | "i8" => Some(SampleFormat::Cs8),
        "cu8" | "u8" => Some(SampleFormat::Cu8),
        "cs16" | "s16" | "iq16" => Some(SampleFormat::Cs16),
        "cf32" | "fc32" | "f32" | "iq32" => Some(SampleFormat::Cf32),
        _ => None,
    }
}

/// What a capture's file name says about it. Raw IQ files carry no header, so
/// DecDVB-T's recorder puts the tuning in the name: `c26_514.000MHz_10Msps_…`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct NameHint {
    pub center_hz: Option<f64>,
    pub sample_rate: Option<f64>,
}

/// Find a centre frequency ("514.000MHz") and a sample rate ("10Msps",
/// "2500ksps") among a file name's `_`-separated parts.
pub fn name_hint(path: &Path) -> NameHint {
    let mut hint = NameHint::default();
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return hint;
    };
    let number = |t: &str, unit: &str| -> Option<f64> {
        // `strip_suffix` wants the exact case, so compare lower-cased.
        let t = t.to_ascii_lowercase();
        t.strip_suffix(unit)?.parse::<f64>().ok()
    };
    for part in stem.split('_') {
        if let Some(v) = number(part, "mhz") {
            hint.center_hz = Some(v * 1e6);
        } else if let Some(v) = number(part, "msps") {
            hint.sample_rate = Some(v * 1e6);
        } else if let Some(v) = number(part, "ksps") {
            hint.sample_rate = Some(v * 1e3);
        }
    }
    hint
}

/// Reads interleaved IQ from a file in fixed-size blocks.
pub struct IqFileReader {
    inner: BufReader<File>,
    fmt: SampleFormat,
    /// Where the samples start and how many bytes of them there are (a WAV
    /// file's data chunk; the whole file otherwise).
    start: u64,
    len: Option<u64>,
    /// Bytes of samples read since `start`.
    pos: u64,
    sample_rate: f64,
    center_freq: f64,
    /// Scratch byte buffer, sized to `block_samples * bytes_per_sample`.
    raw: Vec<u8>,
    name: String,
}

impl IqFileReader {
    /// Open `path` as `fmt`, reading `block_samples` complex samples per
    /// [`IqSource::read`] call. A two-channel WAV file is recognised by its
    /// header, whose format, rate and centre frequency then win over the
    /// arguments; only its data chunk is read.
    pub fn open(
        path: &Path,
        fmt: SampleFormat,
        sample_rate: f64,
        block_samples: usize,
    ) -> Result<Self> {
        let wav = crate::wav::probe(path)?;
        let (fmt, sample_rate, start, len, center) = match &wav {
            Some(w) => (
                w.format,
                w.sample_rate,
                w.data_offset,
                Some(w.data_len),
                w.center_freq.unwrap_or(0.0),
            ),
            None => (fmt, sample_rate, 0, None, 0.0),
        };
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(start))?;
        Ok(IqFileReader {
            inner: BufReader::with_capacity(1 << 20, file),
            fmt,
            start,
            len,
            pos: 0,
            sample_rate,
            center_freq: center,
            raw: vec![0u8; block_samples * fmt.bytes_per_sample()],
            name: path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
        })
    }

    /// Set the centre frequency reported to the GUI (raw files carry no
    /// metadata).
    pub fn with_center_freq(mut self, hz: f64) -> Self {
        self.center_freq = hz;
        self
    }

    /// The sample format being read (a WAV file's, if it is one).
    pub fn format(&self) -> SampleFormat {
        self.fmt
    }
}

impl IqSource for IqFileReader {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        // Read as much of the block as the file has left. `read` may return
        // short reads even mid-file, so loop until the buffer is full or EOF.
        // A WAV file's samples end with its data chunk (chunks may follow).
        let want = match self.len {
            Some(len) => (len.saturating_sub(self.pos)).min(self.raw.len() as u64) as usize,
            None => self.raw.len(),
        };
        let mut filled = 0;
        while filled < want {
            match self.inner.read(&mut self.raw[filled..want])? {
                0 => break,
                n => filled += n,
            }
        }
        self.pos += filled as u64;
        // Drop any trailing partial sample.
        let usable = filled - (filled % self.fmt.bytes_per_sample());
        bytes_to_iq(&self.raw[..usable], self.fmt, out);
        Ok(out.len())
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn center_freq(&self) -> f64 {
        self.center_freq
    }

    fn describe(&self) -> String {
        format!(
            "{} ({:?}, {:.3} MS/s)",
            self.name,
            self.fmt,
            self.sample_rate / 1e6
        )
    }

    fn rewind(&mut self) -> Result<bool> {
        self.inner.seek(SeekFrom::Start(self.start))?;
        self.pos = 0;
        Ok(true)
    }
}

/// Writes interleaved IQ to a file — recording a capture, or the modulator's
/// output when not transmitting live.
pub struct IqFileWriter {
    inner: BufWriter<File>,
    fmt: SampleFormat,
    /// A block's bytes, built here and written in one call: a `write_all`
    /// per sample costs several times the conversion itself.
    bytes: Vec<u8>,
}

impl IqFileWriter {
    pub fn create(path: &Path, fmt: SampleFormat) -> Result<Self> {
        Ok(IqFileWriter {
            inner: BufWriter::with_capacity(1 << 20, File::create(path)?),
            fmt,
            bytes: Vec::new(),
        })
    }

    /// Append samples, clamping to the target format's range.
    pub fn write(&mut self, samples: &[Iq]) -> Result<()> {
        match self.fmt {
            SampleFormat::Cs8 => {
                self.bytes.clear();
                self.bytes.reserve(samples.len() * 2);
                // ×128 undoes `bytes_to_iq`'s ÷128, so a recording of a cs8
                // source is bit-exact. `as i8` on a float saturates, so the
                // clamp is already done for us; `as u8` keeps the bits.
                self.bytes.extend(samples.iter().flat_map(|s| {
                    [
                        (s.re * 128.0).round() as i8 as u8,
                        (s.im * 128.0).round() as i8 as u8,
                    ]
                }));
                self.inner.write_all(&self.bytes)?;
            }
            SampleFormat::Cu8 => {
                for s in samples {
                    let i = (s.re * 128.0 + 127.5).clamp(0.0, 255.0) as u8;
                    let q = (s.im * 128.0 + 127.5).clamp(0.0, 255.0) as u8;
                    self.inner.write_all(&[i, q])?;
                }
            }
            SampleFormat::Cs24 => {
                for s in samples {
                    for v in [s.re, s.im] {
                        let x = (v * 8_388_607.0).clamp(-8_388_608.0, 8_388_607.0) as i32;
                        self.inner.write_all(&x.to_le_bytes()[..3])?;
                    }
                }
            }
            SampleFormat::Cs16 => {
                for s in samples {
                    let i = (s.re * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    let q = (s.im * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    self.inner.write_all(&i.to_le_bytes())?;
                    self.inner.write_all(&q.to_le_bytes())?;
                }
            }
            SampleFormat::Cf32 => {
                for s in samples {
                    self.inner.write_all(&s.re.to_le_bytes())?;
                    self.inner.write_all(&s.im.to_le_bytes())?;
                }
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        self.inner.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_core::Iq;

    #[test]
    fn names_carry_their_tuning() {
        let h = name_hint(Path::new(
            "captures/c26_514.000MHz_10Msps_20261023-094512.cs8",
        ));
        assert_eq!(h.center_hz, Some(514e6));
        assert_eq!(h.sample_rate, Some(10e6));
        let h = name_hint(Path::new("x_2400ksps_482MHz.cs8"));
        assert_eq!((h.center_hz, h.sample_rate), (Some(482e6), Some(2.4e6)));
        assert_eq!(name_hint(Path::new("capture.cs8")), NameHint::default());
    }

    #[test]
    fn extension_guessing() {
        assert_eq!(
            format_from_path(Path::new("cap.cs8")),
            Some(SampleFormat::Cs8)
        );
        assert_eq!(
            format_from_path(Path::new("cap.CF32")),
            Some(SampleFormat::Cf32)
        );
        assert_eq!(format_from_path(Path::new("cap.bin")), None);
    }

    #[test]
    fn write_then_read_round_trip() {
        let dir = std::env::temp_dir();
        let path = dir.join("decdvbt-io-roundtrip.cf32");
        let want: Vec<Iq> = (0..64)
            .map(|k| Iq::new(k as f32 / 64.0, -(k as f32) / 128.0))
            .collect();

        let mut w = IqFileWriter::create(&path, SampleFormat::Cf32).unwrap();
        w.write(&want).unwrap();
        w.finish().unwrap();

        let mut r = IqFileReader::open(&path, SampleFormat::Cf32, 1e6, 64).unwrap();
        let mut got = Vec::new();
        let n = r.read(&mut got).unwrap();
        assert_eq!(n, want.len());
        assert_eq!(got, want);

        // Second read hits EOF.
        assert_eq!(r.read(&mut got).unwrap(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cs8_recording_is_bit_exact() {
        let path = std::env::temp_dir().join("decdvbt-io-cs8-exact.cs8");
        let bytes: Vec<u8> = (0..=255u8).collect();
        let mut src = Vec::new();
        decdvbt_core::bytes_to_iq(&bytes, SampleFormat::Cs8, &mut src);
        let mut w = IqFileWriter::create(&path, SampleFormat::Cs8).unwrap();
        w.write(&src).unwrap();
        w.finish().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn short_final_block_is_truncated_to_whole_samples() {
        let dir = std::env::temp_dir();
        let path = dir.join("decdvbt-io-partial.cs8");
        // 5 bytes = 2 whole cs8 samples + 1 stray byte.
        std::fs::write(&path, [1u8, 2, 3, 4, 5]).unwrap();
        let mut r = IqFileReader::open(&path, SampleFormat::Cs8, 1e6, 16).unwrap();
        let mut got = Vec::new();
        assert_eq!(r.read(&mut got).unwrap(), 2);
        let _ = std::fs::remove_file(&path);
    }
}
