// made by CasualArclamp
//! The Red Button: the playing service's MHEG-5 application
//! (`decdvbt_mheg`), its 720x576 canvas drawn over the picture, and the
//! remote's keys.
//!
//! The red key (R, or the button under the picture) starts it: the hub begins
//! receiving the service's object carousel (`decdvbt_engine::CarouselLink`),
//! the engine boots the application once the carousel's root has come, and
//! the red key is handed to the application once it has settled in a scene (the BBC's
//! starts on its "press red" trigger and opens its menu on red). Keys while
//! it runs: the arrows, Enter (select), Backspace or Esc (back), 0–9, R G Y
//! B for the colour keys and T for Text. The button stops it.
//!
//! The interaction channel is on: an application may fetch files over the
//! internet from the servers its carousel lists (`auth.servers`), and may ask
//! for a stream from there in place of the broadcast, which the player then
//! plays ([`RedButton::take_ic_stream`]). Freeview's internet channels (NHK
//! WORLD, Al Jazeera and the like: data services) work this way. What the
//! applications keep in the receiver's non-volatile store ("pst://", their
//! server's device id) is kept in `%APPDATA%\DecDVB-T\mheg-store.bin`.
//!
//! For screenshots and tests, `DECDVBT_REDBUTTON=100,wait,15` (UK key codes,
//! `wait` a second) starts it on the playing service and presses the keys
//! one a second once the application has a scene (the red key that starts it
//! is pressed for you, so the list need not begin with 100).
//!
//! Cost: nothing at all until it is started; while it runs the engine is
//! ticked with each frame (at least ten a second; a few compares when
//! nothing happens), the hub hands the carousel only the service's DSM-CC
//! packets, and the canvas is redrawn and uploaded only when the application
//! changed it. On the C30 BBC One HD test signal, playing with the BBC News
//! page open costs 59.4 % CPU against 57.4 % without it.

use std::time::{Duration, Instant};

use crate::prefs;

use decdvbt_engine::CarouselLink;
use decdvbt_mheg::ic::{Http, IcFiles, USER_AGENT};
use decdvbt_mheg::{Engine, Renderer, Request, Service, key};
use eframe::egui::{
    self, Color32, ColorImage, Key, Modifiers, Rect, TextureHandle, TextureOptions, pos2,
};

/// How long the application must sit idle in a scene before the red key
/// that started it is pressed.
const SETTLE: Duration = Duration::from_millis(1500);

/// The status of a service whose carousel never came.
const NO_APP: &str = "this service has no interactive application";

/// How often the non-volatile store is looked at for changes to save.
const STORE_EVERY_MS: u64 = 2000;

/// The canvas (D-Book 7 §13.10.1).
const W: f32 = 720.0;
const H: f32 = 576.0;

#[derive(Default)]
pub struct RedButton {
    link: Option<CarouselLink>,
    engine: Option<Engine>,
    renderer: Option<Renderer>,
    texture: Option<TextureHandle>,
    t0: Option<Instant>,
    /// The red key, to give the application once it has settled.
    pending_red: bool,
    /// The scene, and since when it has been the scene with the engine idle.
    settled: Option<(String, Instant)>,
    /// Where the application wants the picture, on its canvas (`None`: full
    /// screen).
    video: Option<(i32, i32, i32, i32)>,
    /// A service of this multiplex the application asked to tune to.
    tune: Option<u16>,
    /// The interaction channel's fetches (made with the engine).
    http: Option<Http>,
    /// A stream the application asked for (Some(None): the broadcast
    /// again), for the player to take.
    ic_stream: Option<Option<String>>,
    /// The non-volatile store as last saved, and when it was last looked at.
    store_saved: Vec<u8>,
    store_at: u64,
    pub status: String,
    /// `DECDVBT_REDBUTTON`'s keys still to press, and when the last was.
    script: Option<Vec<String>>,
    script_read: bool,
    script_at: Option<Instant>,
}

impl RedButton {
    pub fn active(&self) -> bool {
        self.link.is_some()
    }

    /// Whether the application has a scene to draw (a radio service shows
    /// its level meters until it does, and when it has no application).
    pub fn shows_app(&self) -> bool {
        self.engine.as_ref().is_some_and(|e| e.has_scene())
    }

    pub fn service(&self) -> Option<u16> {
        self.link.as_ref().map(|l| l.service)
    }

    /// What the hub should receive (put in its outputs).
    pub fn link(&self) -> Option<CarouselLink> {
        self.link.clone()
    }

    /// Start service `sid`'s application, and press red for it.
    pub fn start(&mut self, sid: u16) {
        self.follow(sid);
        self.pending_red = true;
    }

    /// Run service `sid`'s application from its start (the player moved to
    /// another service while one ran: a receiver boots the new service's).
    pub fn follow(&mut self, sid: u16) {
        self.stop();
        self.link = Some(CarouselLink::new(sid));
        self.t0 = Some(Instant::now());
        self.status = "waiting for the carousel".into();
        if self.renderer.is_none() {
            self.renderer = Some(Renderer::new());
        }
    }

    pub fn stop(&mut self) {
        self.save_store();
        self.link = None;
        self.engine = None;
        self.http = None;
        // An internet stream it started stops with it.
        if self.ic_stream.is_none() {
            self.ic_stream = Some(None);
        }
        self.texture = None;
        self.video = None;
        self.pending_red = false;
        self.settled = None;
        self.tune = None;
        self.status.clear();
    }

    /// Where the picture goes within `rect` (the whole picture's place):
    /// where the application scaled it to, else all of it.
    pub fn video_rect(&self, rect: Rect) -> Rect {
        match self.video {
            Some((x, y, w, h)) if self.active() => {
                let sx = rect.width() / W;
                let sy = rect.height() / H;
                Rect::from_min_size(
                    rect.min + egui::vec2(x as f32 * sx, y as f32 * sy),
                    egui::vec2(w as f32 * sx, h as f32 * sy),
                )
            }
            _ => rect,
        }
    }

    /// Run the engine for this frame and upload its canvas if it changed.
    pub fn update(&mut self, ctx: &egui::Context) {
        let Some(link) = self.link.clone() else {
            return;
        };
        ctx.request_repaint_after(Duration::from_millis(100));
        let now_ms = self.t0.map_or(0, |t| t.elapsed().as_millis() as u64);
        let st = link.state.lock().unwrap();
        let Some(oc) = st.carousel.as_ref() else {
            if st.absent {
                self.status = NO_APP.into();
            }
            return;
        };
        let engine = match &mut self.engine {
            Some(e) => e,
            None => {
                if !oc.has_gateway() {
                    let (done, all) = oc.progress();
                    self.status = format!("receiving the carousel ({done}/{all} modules)");
                    return;
                }
                let mut e = Engine::new();
                // DECDVBT_REDBUTTON_TRACE=1: the engine's trace on stderr.
                e.tracing = std::env::var_os("DECDVBT_REDBUTTON_TRACE").is_some();
                e.trace_stderr = e.tracing;
                if let Some(b) = prefs::mheg_store().and_then(|p| std::fs::read(p).ok()) {
                    e.load_nonvolatile(&b);
                }
                self.store_saved = e.save_nonvolatile();
                e.ic = true;
                let http = self.http.insert(Http::new(USER_AGENT));
                e.boot(&IcFiles { carousel: oc, http });
                self.status.clear();
                self.engine.insert(e)
            }
        };
        let Some(http) = self.http.as_ref() else {
            return;
        };
        let files = IcFiles { carousel: oc, http };
        engine.service = Service {
            original_network_id: st.original_network_id.unwrap_or(0),
            transport_stream_id: st.ts_id.unwrap_or(0),
            service_id: link.service,
            services: st.services.clone(),
        };
        engine.clock_unix = st.utc.map(|(t, at)| t + at.elapsed().as_secs() as i64);
        engine.local_offset_s = st.offset_min as i64 * 60;
        // The red key that started it goes to the application once it has
        // settled in a scene (the BBC's init scene ignores keys; its trigger
        // scene opens the menu on red).
        let scene = engine.scene().map(str::to_string);
        if scene.is_none() || !engine.is_idle() {
            self.settled = None;
        } else if self.settled.as_ref().map(|s| &s.0) != scene.as_ref() {
            self.settled = scene.map(|s| (s, Instant::now()));
        }
        if self.pending_red
            && self
                .settled
                .as_ref()
                .is_some_and(|s| s.1.elapsed() >= SETTLE)
        {
            self.pending_red = false;
            engine.key(key::RED);
        }
        let changed = engine.tick(now_ms, &files);
        for r in engine.requests.drain(..) {
            match r {
                Request::Video(v) => self.video = v,
                Request::IcStream(s) => self.ic_stream = Some(s),
                // TuneIndex: a service of this multiplex is played; others
                // would need a retune, which is the app's to do (not yet).
                Request::Tune(i) => {
                    if let Some(&(_, tsid, sid)) =
                        usize::try_from(i).ok().and_then(|i| st.services.get(i))
                        && Some(tsid) == st.ts_id
                    {
                        self.tune = Some(sid);
                    }
                }
            }
        }
        if now_ms >= self.store_at + STORE_EVERY_MS {
            self.store_at = now_ms;
            let store = engine.save_nonvolatile();
            if store != self.store_saved {
                if let Some(p) = prefs::mheg_store() {
                    let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
                    let _ = std::fs::write(&p, &store);
                }
                self.store_saved = store;
            }
        }
        if let Some(e) = &engine.error {
            self.status = e.clone();
        } else if !engine.has_scene() {
            let (done, all) = oc.progress();
            self.status = format!("starting ({done}/{all} modules in)");
        } else {
            self.status.clear();
        }
        drop(st);
        if changed || self.texture.is_none() {
            let (Some(r), Some(e)) = (self.renderer.as_mut(), self.engine.as_ref()) else {
                return;
            };
            r.render(e);
            let img = ColorImage::from_rgba_unmultiplied([W as usize, H as usize], &r.rgba);
            match &mut self.texture {
                Some(t) => t.set(img, TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("mheg", img, TextureOptions::LINEAR)),
            }
        }
    }

    /// The service the application asked to be tuned to, once.
    pub fn take_tune(&mut self) -> Option<u16> {
        self.tune.take()
    }

    /// The internet stream the application asked to play (Some(None): back
    /// to the broadcast), once.
    pub fn take_ic_stream(&mut self) -> Option<Option<String>> {
        self.ic_stream.take()
    }

    /// Save the non-volatile store now if it changed.
    fn save_store(&mut self) {
        let Some(e) = &self.engine else { return };
        let store = e.save_nonvolatile();
        if store != self.store_saved
            && let Some(p) = prefs::mheg_store()
        {
            let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
            let _ = std::fs::write(&p, &store);
            self.store_saved = store;
        }
    }

    /// Draw the canvas over the picture's `rect`.
    pub fn paint(&self, p: &egui::Painter, rect: Rect) {
        if !self.active() {
            return;
        }
        if let Some(t) = &self.texture {
            p.image(
                t.id(),
                rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        if !self.status.is_empty() {
            p.text(
                rect.left_bottom() + egui::vec2(8.0, -8.0),
                egui::Align2::LEFT_BOTTOM,
                format!("Red Button: {}", self.status),
                egui::FontId::proportional(13.0),
                Color32::from_rgb(230, 230, 230),
            );
        }
    }

    /// A key on the remote (UK codes): to the application, if it runs.
    pub fn key(&mut self, code: i32) {
        if let Some(e) = &mut self.engine {
            e.key(code);
        }
    }

    /// The keyboard as a remote, while the application runs and no text box
    /// has the focus. R starts it when it is not running and `sid` plays.
    pub fn keyboard(&mut self, ctx: &egui::Context, sid: Option<u16>) {
        self.run_script(sid);
        if ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        let pressed = |k: Key| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, k));
        if !self.active() {
            if let Some(s) = sid
                && pressed(Key::R)
            {
                self.start(s);
            }
            return;
        }
        const MAP: [(Key, i32); 21] = [
            (Key::ArrowUp, key::UP),
            (Key::ArrowDown, key::DOWN),
            (Key::ArrowLeft, key::LEFT),
            (Key::ArrowRight, key::RIGHT),
            (Key::Enter, key::SELECT),
            (Key::Backspace, key::CANCEL),
            (Key::Escape, key::CANCEL),
            (Key::R, key::RED),
            (Key::G, key::GREEN),
            (Key::Y, key::YELLOW),
            (Key::B, key::BLUE),
            (Key::T, key::TEXT),
            (Key::Num0, key::DIGIT0),
            (Key::Num1, key::DIGIT0 + 1),
            (Key::Num2, key::DIGIT0 + 2),
            (Key::Num3, key::DIGIT0 + 3),
            (Key::Num4, key::DIGIT0 + 4),
            (Key::Num5, key::DIGIT0 + 5),
            (Key::Num6, key::DIGIT0 + 6),
            (Key::Num7, key::DIGIT0 + 7),
            (Key::Num8, key::DIGIT0 + 8),
        ];
        for (k, code) in MAP {
            if pressed(k) {
                self.key(code);
            }
        }
        if pressed(Key::Num9) {
            self.key(key::DIGIT0 + 9);
        }
    }

    /// `DECDVBT_REDBUTTON`: start, then a key a second.
    fn run_script(&mut self, sid: Option<u16>) {
        if !self.script_read {
            self.script_read = true;
            self.script = std::env::var("DECDVBT_REDBUTTON")
                .ok()
                .map(|s| s.split(',').map(|k| k.trim().to_string()).collect());
        }
        if self.script.is_none() {
            return;
        }
        if !self.active() {
            if let Some(s) = sid {
                self.start(s);
            }
            return;
        }
        let ready = self.engine.as_ref().is_some_and(|e| e.has_scene()) && !self.pending_red;
        if !ready
            || self
                .script_at
                .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        self.script_at = Some(Instant::now());
        let Some(k) = self
            .script
            .as_mut()
            .filter(|s| !s.is_empty())
            .map(|s| s.remove(0))
        else {
            return;
        };
        if let Ok(code) = k.parse() {
            self.key(code);
        }
    }

    /// The remote's buttons, under the picture.
    pub fn remote(&mut self, ui: &mut egui::Ui, sid: Option<u16>) {
        ui.horizontal_wrapped(|ui| {
            let red = Color32::from_rgb(200, 30, 30);
            let label = if self.active() { "■ Red Button" } else { "● Red Button" };
            if ui
                .add(egui::Button::new(egui::RichText::new(label).color(Color32::WHITE)).fill(red))
                .on_hover_text("The service's interactive application (keys: R starts it; arrows, Enter, Backspace, 0-9, R G Y B, T)")
                .clicked()
            {
                if self.active() {
                    self.stop();
                } else if let Some(s) = sid {
                    self.start(s);
                }
            }
            if !self.active() {
                return;
            }
            let colour = |ui: &mut egui::Ui, c: Color32, code: i32, me: &mut Self| {
                if ui.add(egui::Button::new("    ").fill(c)).clicked() {
                    me.key(code);
                }
            };
            colour(ui, red, key::RED, self);
            colour(ui, Color32::from_rgb(30, 160, 60), key::GREEN, self);
            colour(ui, Color32::from_rgb(230, 190, 30), key::YELLOW, self);
            colour(ui, Color32::from_rgb(40, 90, 220), key::BLUE, self);
            for (t, code) in [
                ("◀", key::LEFT),
                ("▲", key::UP),
                ("▼", key::DOWN),
                ("▶", key::RIGHT),
                ("OK", key::SELECT),
                ("Back", key::CANCEL),
                ("Text", key::TEXT),
            ] {
                if ui.button(t).clicked() {
                    self.key(code);
                }
            }
        });
    }
}
