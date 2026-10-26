// made by CasualArclamp
// A release exe is a Windows GUI program: no console window opens with it
// (`attach_console` joins the one that started it, if there is one). Debug
// builds keep theirs, for the log.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! DecDVB-T TX: a video file to a DVB-T signal through a HackRF One.
//!
//! A separate program from the receiver (`decdvbt-gui`): its own window, its
//! own settings file, and the transmit engine in the `decdvbt-tx` library
//! (docs/TX_DESIGN.md). The window starts disarmed on the "test only" output
//! and never transmits by itself: the radio is reached only through the ARM
//! and TRANSMIT buttons (`TxSession::arm`, then `TxSession::transmit`). None of
//! the command-line hooks (`--demo`, `--screenshot`, `--sink null|iq:PATH
//! --start`) can reach it.

mod app;
mod cli;
mod demo;
mod freq_display;
mod host;
mod meters;
mod modeinfo;
mod prefs;
mod safety;
mod selftest;
mod settings;
mod signal_view;
mod source;
mod thread_cpu;
mod waterfall;

use eframe::egui::{self, Color32};

pub(crate) const AMBER: Color32 = Color32::from_rgb(255, 176, 40);
pub(crate) const RED: Color32 = Color32::from_rgb(255, 90, 80);
pub(crate) const GREEN: Color32 = Color32::from_rgb(110, 220, 130);

/// A program built for the Windows GUI subsystem starts with no console and
/// its output goes nowhere. Started from a terminal, join that terminal's
/// console (so `--version`, usage errors and the run summary print) and point
/// the standard handles at it unless they were redirected; started from
/// Explorer there is none to join and nothing happens.
#[cfg(all(windows, not(debug_assertions)))]
fn attach_console() {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
        SetStdHandle,
    };
    // SAFETY: plain Win32 calls with valid arguments; the handles given to
    // `SetStdHandle` are leaked on purpose, as the standard handles are for
    // the life of the process.
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let h = GetStdHandle(which);
            // No handle (-1 is INVALID_HANDLE_VALUE): not redirected, so
            // the console.
            if h.is_null() || h as isize == -1 {
                if let Ok(f) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
                    SetStdHandle(which, f.into_raw_handle() as _);
                }
            }
        }
    }
}

#[cfg(not(all(windows, not(debug_assertions))))]
fn attach_console() {}

/// The window's icon (the same picture as the exe's), from the PNG built in.
fn window_icon() -> Option<egui::IconData> {
    let img = image::load_from_memory(include_bytes!("../assets/icon-256.png")).ok()?;
    let rgba = img.to_rgba8();
    Some(egui::IconData {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

fn main() -> eframe::Result {
    attach_console();
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `--version` alone: print it and stop (as the receiver's does).
    if args.first().map(String::as_str) == Some("--version") {
        println!("decdvbt-tx {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", cli::USAGE);
        return Ok(());
    }
    let opts = match cli::Options::parse(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("decdvbt-tx: {e}");
            eprintln!("{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    if opts.selftest {
        std::process::exit(selftest::run());
    }

    // A screenshot or demo comes out the same each time: it keeps no window
    // size and reads no settings.
    let reproducible = opts.screenshot.is_some() || opts.demo;
    let (w, h) = opts
        .size
        .or_else(|| {
            if reproducible {
                None
            } else {
                settings::Settings::load().window
            }
        })
        .unwrap_or((1280, 820));
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([w as f32, h as f32])
        .with_min_inner_size([900.0, 600.0])
        .with_title("DecDVB-T TX")
        .with_drag_and_drop(true);
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let exit = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
    let exit2 = std::sync::Arc::clone(&exit);
    eframe::run_native(
        "DecDVB-T TX",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            // One monospace face throughout: columns of numbers line up.
            cc.egui_ctx.style_mut_of(egui::Theme::Dark, |s| {
                s.override_text_style = Some(egui::TextStyle::Monospace);
            });
            Ok(Box::new(ExitCarrier {
                app: app::App::new(opts, &cc.egui_ctx),
                exit: exit2,
            }))
        }),
    )?;
    let code = exit.load(std::sync::atomic::Ordering::Relaxed);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Wraps the app so its exit code (a failed `--start` run) outlives the window:
/// `run_native` returns nothing but "ok".
struct ExitCarrier {
    app: app::App,
    exit: std::sync::Arc<std::sync::atomic::AtomicI32>,
}

impl eframe::App for ExitCarrier {
    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        self.app.on_exit(gl);
        self.exit
            .store(self.app.exit_code(), std::sync::atomic::Ordering::Relaxed);
    }

    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.app.logic(ctx, frame);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.app.ui(ui, frame);
    }
}
