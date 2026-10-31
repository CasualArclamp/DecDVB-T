// made by CasualArclamp
//! Playing sound in the app: PCM (decoded by ffmpeg in the TV player) →
//! resample → sound card.
//!
//! The TV player hands each block of decoded PCM to [`AudioPlayer::push_pcm`],
//! which only queues it; a thread of the player's own does the rest, so the
//! sound card never holds up the receiver. The sound card pulls from a ring
//! buffer, filled at its own rate by the resampler.
//!
//! **Buffering.** The decoder hands audio over in bursts, and the
//! broadcaster's clock is not the sound card's. Play
//! starts once [`START_MS`] is buffered; the resampling ratio is then
//! trimmed (by at most 1 %, inaudibly) to hold the level near that, so drift
//! never builds up into an underrun or an ever-growing delay. Running dry
//! re-buffers; a level far over (a burst after a stall) is cut back.
//!
//! **Controls.** Volume and mute are app-wide ([`set_volume`],
//! [`set_muted`]), applied in the sound card's callback so they act at once.
//! So is the output device ([`set_output_device`]): players already running
//! move to a newly chosen one within a tenth of a second.
//! Pause is per player, through its [`AudioHandle`]: paused, the audio is
//! decoded and dropped, so resuming plays live rather than from where it
//! stopped.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::resample::Resampler;

/// Buffered before play starts (and after running dry), milliseconds.
pub const START_MS: u32 = 400;
/// Buffer level beyond which audio is cut back to `START_MS`. The decoder
/// hands over its start-up backlog in one go, which is cut rather than
/// played a second late (the pictures follow the sound).
const MAX_MS: u32 = 800;
/// PCM blocks queued for the player thread before new ones are dropped.
const QUEUE: usize = 512;

static VOLUME: AtomicU32 = AtomicU32::new(0x3F33_3333); // 0.7f32
static MUTED: AtomicBool = AtomicBool::new(false);

/// The gain at the top of the app-wide volume's travel: twice the decoded
/// level (+6 dB), since broadcast sound is mastered quiet. Samples are
/// clipped at full scale.
const MAX_GAIN: f32 = 2.0;

/// Set the app-wide volume, 0..=1 (a slider position; the gain applied is
/// its cube times [`MAX_GAIN`], so the slider's travel follows loudness).
pub fn set_volume(v: f32) {
    VOLUME.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
}

pub fn volume() -> f32 {
    f32::from_bits(VOLUME.load(Ordering::Relaxed))
}

pub fn set_muted(m: bool) {
    MUTED.store(m, Ordering::Relaxed);
}

pub fn muted() -> bool {
    MUTED.load(Ordering::Relaxed)
}

fn gain() -> f32 {
    if muted() {
        0.0
    } else {
        MAX_GAIN * volume().powi(3)
    }
}

/// The sound output chosen by name (`None`: the system's default), and a
/// count of changes, which running players watch to move to it.
static OUTPUT_DEVICE: Mutex<Option<String>> = Mutex::new(None);
static OUTPUT_CHANGES: AtomicU64 = AtomicU64::new(0);

/// Play through the output device of this name (as [`output_devices`] lists
/// it), or the system's default with `None`. App-wide.
pub fn set_output_device(name: Option<String>) {
    let mut d = OUTPUT_DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    if *d != name {
        *d = name;
        OUTPUT_CHANGES.fetch_add(1, Ordering::Relaxed);
    }
}

/// How many times the output device has been changed (a sound maker
/// opened at an older count should reopen).
pub fn output_changes() -> u64 {
    OUTPUT_CHANGES.load(Ordering::Relaxed)
}

pub fn output_device() -> Option<String> {
    OUTPUT_DEVICE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// The sound output devices there are, by name.
pub fn output_devices() -> Vec<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let mut names: Vec<String> = host
        .output_devices()
        .map(|it| {
            it.filter_map(|d| d.description().ok().map(|n| n.name().to_string()))
                .collect()
        })
        .unwrap_or_default();
    names.dedup();
    names
}

/// Where the sound goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// The default sound output device.
    Device,
    /// Nowhere: decoded and counted (tests, and machines with no sound).
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayState {
    /// Opening the sound device, waiting for the first audio.
    #[default]
    Starting,
    Buffering,
    Playing,
    Paused,
    /// Stopped by an error (see `Status::error`).
    Failed,
}

/// What a player is doing, for display.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub state: PlayState,
    /// What is being decoded.
    pub codec: Option<String>,
    /// How the stream is carried.
    pub carriage: String,
    /// The sound device and its rate.
    pub device: Option<String>,
    pub buffer_ms: u32,
    pub underruns: u64,
    /// PCM blocks received.
    pub decoded: u64,
    pub error: Option<String>,
}

struct Ring {
    buf: VecDeque<f32>,
    /// Playing (rather than filling up to the start level).
    primed: bool,
    underruns: u64,
    /// For PCM decoded elsewhere: the media time, ms, of the end of the
    /// newest audio in `buf`. Set under the same lock as the samples go in,
    /// so `end_ms` less what is still buffered is always the time of the
    /// sound about to be played (see [`AudioHandle::position_ms`]).
    end_ms: Option<i64>,
    /// Ring samples (stereo, interleaved) per millisecond at the output's
    /// rate; 0 until the output is open.
    per_ms: f64,
}

struct Shared {
    ring: Mutex<Ring>,
    paused: AtomicBool,
    status: Mutex<Status>,
    /// Peak level per side, f32 bits, decaying.
    level: [AtomicU32; 2],
    /// Stereo frames handed to the output (counted for the null output).
    played: AtomicU64,
    /// Buffer level beyond which audio is cut back to `START_MS`.
    max_ms: u32,
    /// This player's own volume, 0..=1 as a slider position (f32 bits),
    /// on top of the app's: the in-app TV player's slider.
    volume: AtomicU32,
}

/// A handle on a running player: its status, level and pause control.
/// Cheap to clone; it outlives the player harmlessly.
#[derive(Clone)]
pub struct AudioHandle(Arc<Shared>);

impl std::fmt::Debug for AudioHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AudioHandle").field(&self.status()).finish()
    }
}

impl AudioHandle {
    pub fn status(&self) -> Status {
        let mut s = self.0.status.lock().unwrap().clone();
        let ring = self.0.ring.lock().unwrap();
        s.underruns = ring.underruns;
        if s.state != PlayState::Failed {
            s.state = if self.paused() {
                PlayState::Paused
            } else if ring.primed {
                PlayState::Playing
            } else if s.decoded > 0 {
                PlayState::Buffering
            } else {
                PlayState::Starting
            };
        }
        s
    }

    /// The media time, ms, of the sound the output is about to play, for
    /// PCM handed over with [`AudioPlayer::push_pcm_at`]: the end of the
    /// newest audio less what is still buffered. It is worked out from the
    /// ring as it is now, so it moves with the sound card rather than in
    /// steps as audio blocks arrive. `None` before any such audio.
    pub fn position_ms(&self) -> Option<f64> {
        let ring = self.0.ring.lock().unwrap();
        let end = ring.end_ms?;
        (ring.per_ms > 0.0).then(|| end as f64 - ring.buf.len() as f64 / ring.per_ms)
    }

    /// Peak levels (left, right), 0..1, of the decoded audio before volume.
    pub fn levels(&self) -> [f32; 2] {
        [0, 1].map(|i| f32::from_bits(self.0.level[i].load(Ordering::Relaxed)))
    }

    pub fn set_paused(&self, p: bool) {
        self.0.paused.store(p, Ordering::Relaxed);
    }

    pub fn paused(&self) -> bool {
        self.0.paused.load(Ordering::Relaxed)
    }

    /// Stereo frames sent to the output so far.
    pub fn played_frames(&self) -> u64 {
        self.0.played.load(Ordering::Relaxed)
    }

    /// This player's own volume, 0..=1 (a slider position, applied as its
    /// cube like the app's), multiplied with the app's volume and mute.
    pub fn set_volume(&self, v: f32) {
        self.0
            .volume
            .store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.0.volume.load(Ordering::Relaxed))
    }
}

/// What the player thread is handed: decoded audio, interleaved stereo at
/// `rate`, ending at media time `end_ms` when the sender knows it.
struct Pcm {
    rate: u32,
    samples: Vec<f32>,
    end_ms: Option<i64>,
}

/// Plays PCM decoded elsewhere (by ffmpeg, in the TV player).
pub struct AudioPlayer {
    tx: Option<SyncSender<Pcm>>,
    handle: AudioHandle,
    join: Option<JoinHandle<()>>,
    /// Blocks passed on (and dropped because the player fell behind).
    pub forwarded: u64,
    pub dropped: u64,
}

impl AudioPlayer {
    /// Start a player for audio decoded elsewhere, handed over with
    /// [`AudioPlayer::push_pcm`]; `label` says what it is ("from ffmpeg").
    pub fn start_pcm(label: &str, output: OutputKind) -> Result<AudioPlayer, String> {
        let carriage = label.to_string();
        let shared = Arc::new(Shared {
            ring: Mutex::new(Ring {
                buf: VecDeque::new(),
                primed: false,
                underruns: 0,
                end_ms: None,
                per_ms: 0.0,
            }),
            paused: AtomicBool::new(false),
            status: Mutex::new(Status {
                carriage,
                ..Default::default()
            }),
            level: [AtomicU32::new(0), AtomicU32::new(0)],
            played: AtomicU64::new(0),
            max_ms: MAX_MS,
            volume: AtomicU32::new(1.0f32.to_bits()),
        });
        let (tx, rx) = mpsc::sync_channel::<Pcm>(QUEUE);
        let sh = shared.clone();
        let join = std::thread::Builder::new()
            .name("decdvbt-audio".into())
            .spawn(move || run(rx, sh, output))
            .map_err(|e| e.to_string())?;
        Ok(AudioPlayer {
            tx: Some(tx),
            handle: AudioHandle(shared),
            join: Some(join),
            forwarded: 0,
            dropped: 0,
        })
    }

    /// Decoded audio: interleaved stereo samples at `rate`.
    pub fn push_pcm(&mut self, rate: u32, samples: Vec<f32>) {
        self.send(Pcm {
            rate,
            samples,
            end_ms: None,
        });
    }

    /// As [`AudioPlayer::push_pcm`], saying the media time, ms, at which
    /// these samples end, so [`AudioHandle::position_ms`] can tell where
    /// the sound is.
    pub fn push_pcm_at(&mut self, rate: u32, samples: Vec<f32>, end_ms: i64) {
        self.send(Pcm {
            rate,
            samples,
            end_ms: Some(end_ms),
        });
    }

    fn send(&mut self, input: Pcm) {
        let Some(tx) = &self.tx else { return };
        match tx.try_send(input) {
            Ok(()) => self.forwarded += 1,
            Err(TrySendError::Full(_)) => self.dropped += 1,
            Err(TrySendError::Disconnected(_)) => self.tx = None,
        }
    }

    pub fn handle(&self) -> AudioHandle {
        self.handle.clone()
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        // Closing the queue ends the thread, which closes the device.
        self.tx = None;
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// The sound card side: an open stream (kept alive) and its format.
struct Output {
    rate: u32,
    #[allow(dead_code)] // held so the device keeps playing
    stream: Option<cpal::Stream>,
}

fn open_output(kind: OutputKind, sh: &Arc<Shared>) -> Result<(Output, String), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    if kind == OutputKind::Null {
        return Ok((
            Output {
                rate: 48_000,
                stream: None,
            },
            "no output".into(),
        ));
    }
    let host = cpal::default_host();
    // The device chosen, by name; the default when none is (or it has gone).
    let chosen = output_device().and_then(|want| {
        host.output_devices().ok()?.find(|d| {
            d.description()
                .map(|n| n.name() == want.as_str())
                .unwrap_or(false)
        })
    });
    let dev = match chosen {
        Some(d) => d,
        None => host
            .default_output_device()
            .ok_or("no sound output device")?,
    };
    let name = dev
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "sound output".into());
    let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
    let (rate, ch, fmt) = (
        cfg.sample_rate(),
        cfg.channels() as usize,
        cfg.sample_format(),
    );
    let config: cpal::StreamConfig = cfg.into();
    let err = |e| eprintln!("audio output: {e}");
    let stream = match fmt {
        cpal::SampleFormat::F32 => {
            dev.build_output_stream(config, callback::<f32>(sh.clone(), ch, rate), err, None)
        }
        cpal::SampleFormat::I16 => {
            dev.build_output_stream(config, callback::<i16>(sh.clone(), ch, rate), err, None)
        }
        cpal::SampleFormat::U16 => {
            dev.build_output_stream(config, callback::<u16>(sh.clone(), ch, rate), err, None)
        }
        cpal::SampleFormat::I32 => {
            dev.build_output_stream(config, callback::<i32>(sh.clone(), ch, rate), err, None)
        }
        f => return Err(format!("sound device format {f} is not handled")),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((
        Output {
            rate,
            stream: Some(stream),
        },
        format!("{name} · {} kHz", fmt_khz(rate)),
    ))
}

/// "48", "44.1": a sample rate in kHz without trailing zeros.
fn fmt_khz(hz: u32) -> String {
    let s = format!("{:.2}", hz as f64 / 1000.0);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The sound card's callback: stereo frames from the ring to `ch` channels.
fn callback<T>(
    sh: Arc<Shared>,
    ch: usize,
    rate: u32,
) -> impl FnMut(&mut [T], &cpal::OutputCallbackInfo) + Send + 'static
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let start = (rate as usize * START_MS as usize / 1000) * 2;
    move |data: &mut [T], _| {
        let g = gain() * f32::from_bits(sh.volume.load(Ordering::Relaxed)).powi(3);
        let paused = sh.paused.load(Ordering::Relaxed);
        let mut ring = sh.ring.lock().unwrap();
        if !ring.primed && ring.buf.len() >= start && !paused {
            ring.primed = true;
        }
        let mut played = 0u64;
        for frame in data.chunks_mut(ch.max(1)) {
            let (l, r) = if ring.primed && !paused {
                match (ring.buf.pop_front(), ring.buf.pop_front()) {
                    (Some(l), Some(r)) => {
                        played += 1;
                        ((l * g).clamp(-1.0, 1.0), (r * g).clamp(-1.0, 1.0))
                    }
                    _ => {
                        ring.primed = false;
                        ring.underruns += 1;
                        (0.0, 0.0)
                    }
                }
            } else {
                (0.0, 0.0)
            };
            for (c, s) in frame.iter_mut().enumerate() {
                let v = match (ch, c) {
                    (1, _) => 0.5 * (l + r),
                    (_, 0) => l,
                    (_, 1) => r,
                    _ => 0.0,
                };
                *s = T::from_sample(v);
            }
        }
        drop(ring);
        sh.played.fetch_add(played, Ordering::Relaxed);
    }
}

/// The player thread.
fn run(rx: mpsc::Receiver<Pcm>, sh: Arc<Shared>, kind: OutputKind) {
    let mut changes = OUTPUT_CHANGES.load(Ordering::Relaxed);
    let mut out = match open_output(kind, &sh) {
        Ok((o, name)) => {
            sh.status.lock().unwrap().device = Some(name);
            o
        }
        Err(e) => {
            let mut s = sh.status.lock().unwrap();
            s.state = PlayState::Failed;
            s.error = Some(e);
            return;
        }
    };
    let null = kind == OutputKind::Null;
    let mut per_ms = out.rate as f64 * 2.0 / 1000.0; // ring samples per ms
    sh.ring.lock().unwrap().per_ms = per_ms;
    // The media time the newest PCM block ends at, until it is in the ring.
    let mut pcm_end: Option<i64> = None;
    let mut decoded = 0u64;
    let mut codec: Option<String> = None;
    let mut resampler: Option<Resampler> = None;
    let mut resampled = Vec::new();
    let mut fill_ms = START_MS as f64;
    let mut was_paused = false;
    loop {
        // Another output device chosen: move to it. The old stream closes
        // first; what was buffered for it goes (it was at its rate).
        let now = OUTPUT_CHANGES.load(Ordering::Relaxed);
        if now != changes && !null {
            changes = now;
            out.stream = None;
            match open_output(kind, &sh) {
                Ok((o, name)) => {
                    out = o;
                    per_ms = out.rate as f64 * 2.0 / 1000.0;
                    resampler = None;
                    fill_ms = START_MS as f64;
                    let mut ring = sh.ring.lock().unwrap();
                    ring.per_ms = per_ms;
                    ring.buf.clear();
                    ring.primed = false;
                    drop(ring);
                    let mut s = sh.status.lock().unwrap();
                    s.device = Some(name);
                    s.error = None;
                }
                Err(e) => {
                    let mut s = sh.status.lock().unwrap();
                    s.state = PlayState::Failed;
                    s.error = Some(e);
                    return;
                }
            }
        }
        // `Option<Pcm>`: `None` when nothing came within the timeout, so
        // the status below still refreshes.
        let block = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(b) => Some(b),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let paused = sh.paused.load(Ordering::Relaxed);
        if paused && !was_paused {
            let mut ring = sh.ring.lock().unwrap();
            ring.buf.clear();
            ring.primed = false;
        }
        was_paused = paused;
        // `'play: { … }` is a labelled block: `break 'play` leaves it early
        // (paused, or no sound card) and still reaches the status below.
        if let Some(block) = block {
            'play: {
                decoded += 1;
                pcm_end = block.end_ms.or(pcm_end);
                if codec.is_none() {
                    codec = Some(format!("PCM {} kHz", fmt_khz(block.rate)));
                }
                // Peak meter, decaying ~20 dB a second at typical block rates.
                for side in 0..2 {
                    let peak = block.samples[side..]
                        .iter()
                        .step_by(2)
                        .fold(0f32, |m, v| m.max(v.abs()));
                    let old = f32::from_bits(sh.level[side].load(Ordering::Relaxed));
                    sh.level[side].store(peak.max(old * 0.93).to_bits(), Ordering::Relaxed);
                }
                if paused {
                    break 'play;
                }
                if resampler.as_ref().is_none_or(|r| r.rates().0 != block.rate) {
                    resampler = Some(Resampler::new(block.rate, out.rate));
                }
                let rs = resampler.as_mut().unwrap();
                // Hold the buffer near the start level: over it, consume input
                // a touch faster.
                rs.set_ratio(1.0 + 0.01 * (fill_ms - START_MS as f64) / START_MS as f64);
                resampled.clear();
                rs.process(&block.samples, &mut resampled);
                if null {
                    sh.played
                        .fetch_add(resampled.len() as u64 / 2, Ordering::Relaxed);
                    break 'play;
                }
                let mut ring = sh.ring.lock().unwrap();
                ring.buf.extend(resampled.iter().copied());
                if let Some(e) = pcm_end.take() {
                    ring.end_ms = Some(e);
                }
                let level_ms = ring.buf.len() as f64 / per_ms;
                if level_ms > sh.max_ms as f64 {
                    let cut = (ring.buf.len() - (START_MS as f64 * per_ms) as usize) & !1;
                    ring.buf.drain(..cut);
                }
                let level_ms = ring.buf.len() as f64 / per_ms;
                if ring.primed {
                    fill_ms += 0.05 * (level_ms - fill_ms);
                } else {
                    fill_ms = START_MS as f64;
                }
            }
        }
        let buffered = sh.ring.lock().unwrap().buf.len() as f64 / per_ms;
        let mut s = sh.status.lock().unwrap();
        s.buffer_ms = buffered as u32;
        s.codec = codec.clone();
        s.decoded = decoded;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plays_pcm_handed_to_it() {
        let mut p = AudioPlayer::start_pcm("test", OutputKind::Null).unwrap();
        let h = p.handle();
        for _ in 0..20 {
            p.push_pcm(48_000, vec![0.25; 2 * 960]);
        }
        let t0 = std::time::Instant::now();
        while h.played_frames() < 15 * 960 && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(h.played_frames() >= 15 * 960, "{}", h.played_frames());
        assert_eq!(h.status().carriage, "test");
        assert!(
            h.status().codec.is_some_and(|c| c.contains("PCM")),
            "{:?}",
            h.status()
        );
    }

    #[test]
    fn the_output_device_is_chosen_by_name() {
        // Listing must not fail on a machine with no sound (CI).
        let devices = output_devices();
        eprintln!("output devices: {devices:?}");
        let before = OUTPUT_CHANGES.load(Ordering::Relaxed);
        set_output_device(Some("Speakers (test)".into()));
        assert_eq!(output_device().as_deref(), Some("Speakers (test)"));
        // The same choice again is no change for running players.
        set_output_device(Some("Speakers (test)".into()));
        set_output_device(None);
        assert_eq!(output_device(), None);
        assert_eq!(OUTPUT_CHANGES.load(Ordering::Relaxed), before + 2);
    }

    #[test]
    fn volume_is_cubed_and_mute_wins() {
        set_volume(0.5);
        assert!((gain() - 0.125 * MAX_GAIN).abs() < 1e-6);
        set_volume(1.0);
        assert!((gain() - MAX_GAIN).abs() < 1e-6);
        set_muted(true);
        assert_eq!(gain(), 0.0);
        set_muted(false);
        set_volume(0.7);
    }
}
