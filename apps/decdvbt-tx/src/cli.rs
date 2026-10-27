// made by CasualArclamp
//! The command line, hand-rolled like the receiver GUI's (usage on error).
//!
//! ```text
//! decdvbt-tx [FILE] [--mode 64qam-2/3-8k-1/32] [--channel C30]
//!            [--t2 8k-1/16-16qam-3/4] [--freq-mhz 433.25]
//!            [--sink null|iq:PATH [--start] [--run-secs N]]
//!            [--demo [--state ready]] [--screenshot OUT.png [--after SECS]]
//!            [--selftest] [--version]
//! ```
//!
//! These are the window's test hooks, and none of them can reach the radio:
//! there is no `--sink hackrf`, and `--start` (which prepares and then
//! transmits by itself) is refused without a `null` or `iq:` sink. Opening a
//! file, a mode or a channel only fills in the form.

use std::path::PathBuf;

use decdvbt_core::channel::{parse_channel, uhf_centre_hz};
use decdvbt_core::dvbt::Mode;

use crate::demo::DemoState;
use decdvbt_tx::modes::T2Config;

pub const USAGE: &str = concat!(
    "usage: decdvbt-tx [FILE] [--mode 64qam-2/3-8k-1/32] [--channel C30]\n",
    "                  [--t2 8k-1/16-16qam-3/4] [--freq-mhz 433.25]\n",
    "                  [--sink null|iq:PATH [--start] [--run-secs N]]\n",
    "                  [--demo [--state STATE]] [--screenshot OUT.png [--after SECS]]\n",
    "                  [--size WxH] [--modes] [--selftest] [--version]\n",
    "  --start        prepare, then transmit by itself: only with --sink null or iq:PATH\n",
    "  --run-secs N   with --start: stop after N seconds of signal (default 30), print\n",
    "                 the counters and exit\n",
    "  --demo         draw a made-up status and spectrum; no session, no ffmpeg, no radio\n",
    "  --state STATE  with --demo: idle|preparing|ready|armed|transmitting|running|\n",
    "                 stuffing|trouble|failed|finished\n",
    "  --mode M       a DVB-T mode; ...-1mhz is DVB-T at 1 MHz\n",
    "  --t2 M         DVB-T2 at 1.7 MHz: FFT-GUARD-CONSTELLATION-RATE (1k to 8k)\n",
    "  --freq-mhz F   the centre, MHz (anywhere 1 MHz to 6 GHz)\n",
    "  --modes        open the All modes window at the start\n",
    "  --selftest     check the safety logic and the defaults, print the result, exit"
);

/// Where the output goes, from `--sink`.
#[derive(Debug, Clone, PartialEq)]
pub enum SinkArg {
    Null,
    IqFile(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Options {
    pub file: Option<PathBuf>,
    pub mode: Option<Mode>,
    /// `--t2`: send DVB-T2 at 1.7 MHz in this mode.
    pub t2: Option<T2Config>,
    pub centre_hz: Option<f64>,
    pub sink: Option<SinkArg>,
    pub start: bool,
    pub run_secs: Option<f64>,
    pub demo: bool,
    pub state: Option<DemoState>,
    pub screenshot: Option<PathBuf>,
    pub after: Option<f64>,
    pub size: Option<(u32, u32)>,
    pub selftest: bool,
    /// Open the "All modes" window at the start (for screenshots).
    pub show_modes: bool,
}

impl Options {
    /// Parse `args` (without the program name).
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Options, String> {
        let mut o = Options::default();
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            let mut value = |what: &str| it.next().ok_or(format!("{a} needs {what}"));
            match a.as_str() {
                "--mode" => {
                    let m = Mode::parse(&value("a mode, like 64qam-2/3-8k-1/32")?)?;
                    if m.hierarchical() {
                        return Err("hierarchical modes are not supported".into());
                    }
                    o.mode = Some(m);
                }
                "--t2" => {
                    let t = value("a T2 mode, like 8k-1/16-16qam-3/4")?;
                    o.t2 = Some(
                        crate::settings::parse_t2(&t)
                            .ok_or(format!("not a 1.7 MHz T2 mode the app offers: {t}"))?,
                    );
                }
                "--freq-mhz" => {
                    let f: f64 = value("MHz")?
                        .parse()
                        .map_err(|_| "--freq-mhz needs a number".to_string())?;
                    if !(1.0..=6000.0).contains(&f) {
                        return Err("--freq-mhz must be 1 to 6000".into());
                    }
                    o.centre_hz = Some(f * 1e6);
                }
                "--channel" => {
                    let t = value("a channel, like C30")?;
                    let n = parse_channel(&t).ok_or(format!("not a UHF channel (21-69): {t}"))?;
                    o.centre_hz = uhf_centre_hz(n);
                }
                "--sink" => {
                    let t = value("null or iq:PATH")?;
                    o.sink = Some(match t.as_str() {
                        "null" => SinkArg::Null,
                        s if s.starts_with("iq:") && s.len() > 3 => {
                            SinkArg::IqFile(PathBuf::from(&s[3..]))
                        }
                        "hackrf" => {
                            return Err("there is no --sink hackrf: choose the HackRF output \
                                        in the window and use ARM, then TRANSMIT"
                                .into());
                        }
                        other => return Err(format!("not a sink: {other} (null or iq:PATH)")),
                    });
                }
                "--start" => o.start = true,
                "--run-secs" => {
                    let s: f64 = value("seconds")?
                        .parse()
                        .map_err(|_| "--run-secs needs a number".to_string())?;
                    if !(s > 0.0 && s <= 86_400.0) {
                        return Err("--run-secs must be 0 to 86400".into());
                    }
                    o.run_secs = Some(s);
                }
                "--demo" => o.demo = true,
                "--state" => {
                    let t = value("a state")?;
                    o.state = Some(
                        DemoState::parse(&t)
                            .ok_or(format!("not a demo state: {t} ({})", DemoState::NAMES))?,
                    );
                }
                "--screenshot" => o.screenshot = Some(value("a .png path")?.into()),
                "--after" => {
                    o.after = Some(
                        value("seconds")?
                            .parse()
                            .map_err(|_| "--after needs a number".to_string())?,
                    );
                }
                "--size" => {
                    let t = value("WxH, like 1280x800")?;
                    let (w, h) = t.split_once('x').ok_or("--size needs WxH")?;
                    let (w, h): (u32, u32) = (
                        w.parse().map_err(|_| "--size needs WxH".to_string())?,
                        h.parse().map_err(|_| "--size needs WxH".to_string())?,
                    );
                    if !(900..=8000).contains(&w) || !(600..=5000).contains(&h) {
                        return Err("--size is at least 900x600".into());
                    }
                    o.size = Some((w, h));
                }
                "--selftest" => o.selftest = true,
                "--modes" => o.show_modes = true,
                s if s.starts_with("--") => return Err(format!("unknown option {s}")),
                // The one file. Only the form is filled in: nothing starts.
                _ => {
                    if o.file.replace(PathBuf::from(&a)).is_some() {
                        return Err("only one file".into());
                    }
                }
            }
        }
        if o.start && o.sink.is_none() {
            return Err("--start needs --sink null or --sink iq:PATH".into());
        }
        if o.start && o.demo {
            return Err("--start and --demo do not go together".into());
        }
        if o.start && o.file.is_none() {
            return Err("--start needs a FILE".into());
        }
        if o.state.is_some() && !o.demo {
            return Err("--state needs --demo".into());
        }
        Ok(o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Options, String> {
        Options::parse(s.split_whitespace().map(String::from))
    }

    #[test]
    fn a_file_alone_only_fills_the_form() {
        let o = parse("clip.mp4").unwrap();
        assert_eq!(o.file, Some(PathBuf::from("clip.mp4")));
        assert!(!o.start);
        assert!(o.sink.is_none());
    }

    #[test]
    fn the_narrow_channels_and_a_free_frequency() {
        let o = parse("--mode qpsk-1/2-2k-1/8-1mhz --freq-mhz 433.25").unwrap();
        assert_eq!(
            o.mode.unwrap().bandwidth,
            decdvbt_core::dvbt::Bandwidth::Mhz1
        );
        assert_eq!(o.centre_hz, Some(433.25e6));
        let o = parse("--t2 8k-1/16-16qam-3/4").unwrap();
        assert_eq!(
            o.t2.unwrap().bandwidth,
            decdvbt_core::dvbt::Bandwidth::Mhz1_7
        );
        assert!(parse("--t2 32k-1/128-256qam-2/3").is_err());
        assert!(parse("--freq-mhz 0.5").is_err());
    }

    #[test]
    fn mode_and_channel() {
        let o = parse("--mode 16qam-3/4-8k-1/16 --channel C30").unwrap();
        assert_eq!(o.mode, Some(Mode::parse("16qam-3/4-8k-1/16").unwrap()));
        assert_eq!(o.centre_hz, Some(546e6));
        assert!(parse("--channel C99").is_err());
        assert!(parse("--mode nonsense").is_err());
    }

    #[test]
    fn there_is_no_hackrf_sink() {
        let e = parse("clip.mp4 --sink hackrf --start").unwrap_err();
        assert!(e.contains("no --sink hackrf"), "{e}");
        assert!(parse("--sink radio").is_err());
        assert!(parse("--sink iq:").is_err());
    }

    #[test]
    fn start_needs_a_test_sink_and_a_file() {
        assert!(parse("clip.mp4 --start").is_err());
        assert!(parse("--sink null --start").is_err());
        let o = parse("clip.mp4 --sink null --start --run-secs 10").unwrap();
        assert!(o.start);
        assert_eq!(o.sink, Some(SinkArg::Null));
        assert_eq!(o.run_secs, Some(10.0));
        let o = parse("clip.mp4 --sink iq:out/x.cs8 --start").unwrap();
        assert_eq!(o.sink, Some(SinkArg::IqFile("out/x.cs8".into())));
    }

    #[test]
    fn demo_options() {
        let o = parse("--demo --state transmitting --screenshot out/tx.png --after 1.5").unwrap();
        assert!(o.demo);
        assert_eq!(o.state, Some(DemoState::Transmitting));
        assert_eq!(o.after, Some(1.5));
        assert!(parse("--state ready").is_err());
        assert!(parse("--demo --state hackrf").is_err());
        assert!(parse("clip.mp4 --sink null --start --demo").is_err());
    }

    #[test]
    fn junk_is_refused() {
        assert!(parse("--bogus").is_err());
        assert!(parse("a.mp4 b.mp4").is_err());
        assert!(parse("--size 100x100").is_err());
        assert_eq!(parse("--size 1280x800").unwrap().size, Some((1280, 800)));
        assert!(parse("--mode").is_err());
    }
}
