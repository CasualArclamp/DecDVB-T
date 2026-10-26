// made by CasualArclamp
//! The thread that owns the [`TxSession`], so the window never waits on it.
//!
//! `prepare()` may look for ffmpeg and run test encodes, `transmit()` opens
//! the HackRF over USB and `stop()` joins threads and kills ffmpeg: each can
//! take a few hundred milliseconds, and a frame must not. The window sends
//! commands down a channel; this thread runs them, and every 40 ms publishes a
//! [`TxStatus`] snapshot and the new spectrum rows for the window to read.
//!
//! Rust notes: the session is created *inside* this thread, so it never has to
//! be `Send`. Every call into the library is wrapped in `catch_unwind`, so a
//! panic there becomes an error message in the window instead of a dead
//! thread (a session that panics is dropped, which stops the radio).
//!
//! Safety: nothing here arms or transmits by itself. `Cmd::Arm` carries a
//! `LicenceAck` that only the window can make (from the text it displayed),
//! and `Cmd::Transmit` is sent only from the TRANSMIT button, or for the
//! `--start` test hook with a non-radio sink.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decdvbt_tx::{LicenceAck, PreviewRow, TxConfig, TxSession, TxStatus};
use eframe::egui;

/// How often the status is published, and how long a command waits for it.
const TICK: Duration = Duration::from_millis(40);
/// Spectrum rows kept for the window (20 a second: about 12 s).
const MAX_ROWS: usize = 256;
/// The meters are redrawn at least this often while a session runs.
const METER_PERIOD: Duration = Duration::from_millis(250);

/// What the window asks for.
pub enum Cmd {
    /// Make a session from this config and `prepare()` it. The radio stays off.
    Prepare(Box<TxConfig>),
    Arm(LicenceAck),
    Disarm,
    Transmit,
    /// Stop everything and forget the session.
    Stop,
    SetVga(u16),
    Quit,
}

/// Something the window should tell the user.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    pub text: String,
    /// It came from starting the radio (the window adds "close whatever holds
    /// the HackRF" to those).
    pub from_radio_start: bool,
}

#[derive(Default)]
struct Shared {
    /// `None` while there is no session.
    status: Option<TxStatus>,
    rows: VecDeque<PreviewRow>,
    problems: Vec<Problem>,
    /// The thread has ended (it should only do that on `Quit`).
    dead: bool,
}

/// The window's end of the thread.
pub struct Host {
    cmds: Sender<Cmd>,
    shared: Arc<Mutex<Shared>>,
    thread: Option<JoinHandle<()>>,
}

impl Host {
    pub fn spawn(ctx: egui::Context) -> Host {
        let (cmds, rx) = channel();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let s2 = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("tx-host".into())
            .spawn(move || {
                // Mark the end however the thread ends.
                struct Dead(Arc<Mutex<Shared>>);
                impl Drop for Dead {
                    fn drop(&mut self) {
                        if let Ok(mut s) = self.0.lock() {
                            s.dead = true;
                        }
                    }
                }
                let _dead = Dead(Arc::clone(&s2));
                run(rx, s2, ctx);
            })
            .ok();
        Host {
            cmds,
            shared,
            thread,
        }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmds.send(cmd);
    }

    /// The latest status, `None` when there is no session.
    pub fn status(&self) -> Option<TxStatus> {
        self.shared.lock().ok()?.status.clone()
    }

    /// Spectrum rows since the last call, oldest first.
    pub fn take_rows(&self, out: &mut Vec<PreviewRow>) {
        if let Ok(mut s) = self.shared.lock() {
            out.extend(s.rows.drain(..));
        }
    }

    pub fn take_problems(&self) -> Vec<Problem> {
        self.shared
            .lock()
            .map(|mut s| std::mem::take(&mut s.problems))
            .unwrap_or_default()
    }

    /// The thread is gone (it panicked outside the library calls).
    pub fn is_dead(&self) -> bool {
        self.shared.lock().map_or(true, |s| s.dead)
    }

    /// Stop the session and end the thread, waiting for it: when this returns
    /// the radio is off. Used on exit.
    pub fn shutdown(&mut self) {
        let _ = self.cmds.send(Cmd::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Run `f`, turning a panic into its message.
fn guarded<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|p| {
        let msg = p
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown".into());
        format!("internal error in the transmit library: {msg}")
    })
}

fn problem(shared: &Mutex<Shared>, text: String, from_radio_start: bool) {
    if let Ok(mut s) = shared.lock() {
        s.problems.push(Problem {
            text,
            from_radio_start,
        });
    }
}

fn run(rx: Receiver<Cmd>, shared: Arc<Mutex<Shared>>, ctx: egui::Context) {
    let mut session: Option<TxSession> = None;
    let mut last_repaint = Instant::now();
    loop {
        let cmd = match rx.recv_timeout(TICK) {
            Ok(Cmd::Quit) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
        };
        let had_cmd = cmd.is_some();
        if let Some(cmd) = cmd {
            handle(cmd, &mut session, &shared);
        }
        // Publish.
        let mut failed = None;
        if let Some(s) = session.as_mut() {
            match guarded(|| {
                let status = s.status();
                let mut rows = Vec::new();
                s.take_preview_rows(&mut rows);
                (status, rows)
            }) {
                Ok((status, rows)) => {
                    let has_rows = !rows.is_empty();
                    let changed = had_cmd || shared_state_changed(&shared, &status);
                    if let Ok(mut sh) = shared.lock() {
                        sh.status = Some(status);
                        sh.rows.extend(rows);
                        while sh.rows.len() > MAX_ROWS {
                            sh.rows.pop_front();
                        }
                    }
                    // The window repaints for new spectrum rows (20 a second)
                    // and for a changed state; else a few times a second for
                    // the meters. An idle window costs nothing.
                    if has_rows || changed || last_repaint.elapsed() >= METER_PERIOD {
                        last_repaint = Instant::now();
                        wake(&ctx);
                    }
                }
                Err(e) => failed = Some(e),
            }
        } else if had_cmd {
            wake(&ctx);
        }
        if let Some(e) = failed {
            problem(&shared, e, false);
            end_session(&mut session, &shared);
            wake(&ctx);
        }
    }
    // Exit (or the window is gone): the radio goes off before this returns.
    end_session(&mut session, &shared);
}

/// Stop and drop the session, whatever state it is in.
fn end_session(session: &mut Option<TxSession>, shared: &Mutex<Shared>) {
    if let Some(mut s) = session.take() {
        if let Err(e) = guarded(|| s.stop()) {
            problem(shared, e, false);
        }
        // `Drop` stops again (harmless) and joins what is left.
        if let Err(e) = guarded(|| drop(s)) {
            problem(shared, e, false);
        }
    }
    if let Ok(mut sh) = shared.lock() {
        sh.status = None;
        sh.rows.clear();
    }
}

fn handle(cmd: Cmd, session: &mut Option<TxSession>, shared: &Mutex<Shared>) {
    match cmd {
        Cmd::Prepare(cfg) => {
            if session.is_some() {
                problem(shared, "a session is already running".into(), false);
                return;
            }
            let made = guarded(|| {
                let mut s = TxSession::new(*cfg)?;
                s.prepare()?;
                Ok::<TxSession, decdvbt_tx::Error>(s)
            });
            match made {
                Ok(Ok(s)) => *session = Some(s),
                Ok(Err(e)) => problem(shared, e.to_string(), false),
                Err(e) => problem(shared, e, false),
            }
        }
        Cmd::Arm(ack) => {
            if let Some(s) = session.as_mut()
                && let Err(e) = guarded(|| s.arm(ack))
            {
                problem(shared, e, false);
            }
        }
        Cmd::Disarm => {
            if let Some(s) = session.as_mut() {
                let _ = guarded(|| s.disarm());
            }
        }
        Cmd::Transmit => {
            if let Some(s) = session.as_mut() {
                let radio = s.config().sink.is_radio();
                match guarded(|| s.transmit()) {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => problem(shared, e.to_string(), radio),
                    Err(e) => problem(shared, e, radio),
                }
            }
        }
        Cmd::Stop => end_session(session, shared),
        Cmd::SetVga(db) => {
            if let Some(s) = session.as_mut() {
                match guarded(|| s.set_txvga(db)) {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => problem(shared, format!("TX VGA: {e}"), false),
                    Err(e) => problem(shared, e, false),
                }
            }
        }
        Cmd::Quit => {}
    }
}

/// The new status differs from the published one in something the window
/// shows besides the figures: the state, the arm, the error.
fn shared_state_changed(shared: &Mutex<Shared>, new: &TxStatus) -> bool {
    shared.lock().ok().is_none_or(|s| match &s.status {
        None => true,
        Some(old) => {
            old.state != new.state
                || old.armed != new.armed
                || old.radiating != new.radiating
                || old.error != new.error
        }
    })
}

/// Ask the window for a frame. Not `request_repaint()`: with no delay egui
/// draws every request twice ("to let things settle"), which would double the
/// window's CPU cost at the 20 spectrum rows a second; a 1 ms delay draws it
/// once.
fn wake(ctx: &egui::Context) {
    ctx.request_repaint_after(Duration::from_millis(1));
}
