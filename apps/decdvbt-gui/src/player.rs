// made by CasualArclamp
//! The in-app player window: one service of the multiplex, decoded by ffmpeg
//! (`decdvbt_video`), its sound through the app's audio output.
//!
//! The hub hands the service's packets to the player through a tap; the app
//! makes the tap and starts the player ([`TvWindow::start`]), and gives the
//! tap up when the window closes. A service with no picture (radio) shows its
//! name, what is playing and the sound's levels. DVB subtitles are drawn over
//! the picture when a subtitle stream is chosen. The service's Red Button
//! application ([`crate::redbutton`]) draws over the picture.
//!
//! Format after DecSAT's player window.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use decdvbt_ts::{Programme, TS_LEN};
use decdvbt_video::{Deinterlace, Selection, VideoPlayer};
use eframe::egui::{
    self, Color32, ColorImage, CornerRadius, Rect, RichText, Sense, TextureHandle, TextureOptions,
    Ui, pos2, vec2,
};

use crate::prefs;
use crate::redbutton::RedButton;
use crate::services::Kind;

/// What the window asks of the app.
pub enum Request {
    /// Closed: give the tap up.
    Close,
    /// Play this service with this sound and these subtitles (`None`: the
    /// first sound; none): a new tap and player.
    Play(u16, Option<u16>, Option<u16>),
}

/// What a service needs: its picture, its sound (`audio` if it is one of its
/// streams, else the first), its subtitles (`subs` if it is a stream of it),
/// and of the multiplex's packets only those of the PAT, its PMT and PCR and
/// the streams chosen. The rest (the other sound, the data carousels of a red
/// button service, a megabit or two) would only be fed to ffmpeg to be
/// thrown away.
pub fn selection(p: &Programme, audio: Option<u16>, subs: Option<u16>) -> Selection {
    let video = p.streams.iter().find(|e| e.is_video()).map(|e| e.pid);
    let audio = audio
        .filter(|a| p.streams.iter().any(|e| e.pid == *a))
        .or_else(|| p.streams.iter().find(|e| e.is_audio()).map(|e| e.pid));
    let subs = subs.filter(|s| subtitle_streams(p).any(|e| e.pid == *s));
    let mut pids = vec![0, p.pmt_pid];
    pids.extend(p.pcr_pid);
    pids.extend([video, audio, subs].into_iter().flatten());
    pids.sort_unstable();
    pids.dedup();
    Selection {
        pids,
        video,
        audio,
        subs,
        deint: Deinterlace::default(),
        gpu: false,
    }
}

/// A service's DVB subtitle streams (teletext subtitles are not drawn).
pub fn subtitle_streams(p: &Programme) -> impl Iterator<Item = &decdvbt_ts::EsInfo> {
    p.streams.iter().filter(|e| e.kind == Some("subtitles"))
}

pub struct TvWindow {
    pub open: bool,
    service: Option<u16>,
    audio: Option<u16>,
    subs: Option<u16>,
    player: Option<VideoPlayer>,
    texture: Option<TextureHandle>,
    /// Why it is not playing (ffmpeg not found, …).
    error: Option<String>,
    /// The ffmpeg path being typed.
    ffmpeg_edit: String,
    /// The window's own volume, 0..=1.
    volume: f32,
    /// Deinterlace bob ×2 (a picture per field) rather than per frame.
    bob: bool,
    /// Decode the video on the graphics card.
    gpu: bool,
    /// Packets the hub could not hand to the player, as the last view said.
    tap_dropped: u64,
    /// The service being played has a picture (not radio).
    picture: bool,
    /// The service's interactive application.
    pub red: RedButton,
}

impl Default for TvWindow {
    fn default() -> Self {
        TvWindow {
            open: false,
            service: None,
            audio: None,
            subs: None,
            player: None,
            texture: None,
            error: None,
            ffmpeg_edit: String::new(),
            volume: prefs::volume(),
            bob: prefs::bob(),
            gpu: prefs::gpu(),
            tap_dropped: 0,
            picture: false,
            red: RedButton::default(),
        }
    }
}

impl TvWindow {
    /// Play service `p` from the tap's receiving end `rx`.
    pub fn start(
        &mut self,
        p: &Programme,
        audio: Option<u16>,
        subs: Option<u16>,
        rx: Receiver<Vec<[u8; TS_LEN]>>,
    ) {
        self.stop();
        self.open = true;
        if self.red.service().is_some_and(|s| s != p.number) {
            self.red.stop();
        }
        self.service = Some(p.number);
        let mut sel = selection(p, audio, subs);
        sel.deint = if self.bob {
            Deinterlace::Bob
        } else {
            Deinterlace::Frame
        };
        sel.gpu = self.gpu;
        self.audio = sel.audio;
        self.subs = sel.subs;
        self.picture = sel.video.is_some();
        let configured = prefs::ffmpeg_path();
        let Some(exe) = decdvbt_video::find_ffmpeg(configured.as_deref()) else {
            self.error = Some(
                "ffmpeg was not found. Install it (winget install Gyan.FFmpeg) \
                 or give its path below."
                    .into(),
            );
            return;
        };
        match VideoPlayer::start(&exe, rx, sel, decdvbt_audio::OutputKind::Device) {
            Ok(v) => {
                v.set_volume(self.volume);
                self.player = Some(v);
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Stop playing (the window stays as it is).
    pub fn stop(&mut self) {
        self.player = None;
        self.error = None;
    }

    /// Whether service `sid` is playing now.
    pub fn is_playing(&self, sid: u16) -> bool {
        self.player.is_some() && self.service == Some(sid)
    }

    pub fn close(&mut self) {
        self.stop();
        self.red.stop();
        self.open = false;
        self.service = None;
        self.texture = None;
    }

    /// The display rate the pictures need while one is playing: the app's
    /// pacing follows it (each frame shows the picture due, so the rate must
    /// be at least theirs). `None` with no picture to show.
    pub fn wants_fps(&self) -> Option<f64> {
        (self.open && self.player.is_some() && self.picture).then_some(if self.bob {
            50.0
        } else {
            30.0
        })
    }

    /// Draw the window; `programmes` are the services in the order of the
    /// list.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        programmes: &[&Programme],
        tap_dropped: u64,
    ) -> Option<Request> {
        if !self.open {
            return None;
        }
        self.tap_dropped = tap_dropped;
        self.red.update(ctx);
        let playing = self.service.filter(|_| self.player.is_some());
        self.red.keyboard(ctx, playing);
        // The picture due now.
        if let Some(f) = self.player.as_mut().and_then(|p| p.poll()) {
            let img =
                ColorImage::from_rgba_premultiplied([f.width as usize, f.height as usize], &f.rgba);
            match &mut self.texture {
                Some(t) => t.set(img, TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("tv", img, TextureOptions::LINEAR)),
            }
        }
        let current = programmes
            .iter()
            .copied()
            .find(|p| Some(p.number) == self.service);
        let title = current
            .and_then(|p| p.name.clone())
            .unwrap_or_else(|| format!("service {}", self.service.unwrap_or(0)));
        let mut req = None;
        let mut open = true;
        egui::Window::new(format!("▶ {title}"))
            .id(egui::Id::new("tv_window"))
            .open(&mut open)
            .default_size([980.0, 660.0])
            .resizable(true)
            .show(ctx, |ui| {
                self.controls(ui, programmes, current, &mut req);
                self.red.remote(ui, playing);
                if current.is_some_and(|p| Kind::of(p) == Kind::Radio) && !self.red.active() {
                    self.radio(ui, current);
                } else {
                    self.picture(ui);
                }
                self.details(ui);
            });
        if !open {
            self.close();
            return Some(Request::Close);
        }
        req
    }

    fn controls(
        &mut self,
        ui: &mut Ui,
        programmes: &[&Programme],
        current: Option<&Programme>,
        req: &mut Option<Request>,
    ) {
        ui.horizontal_wrapped(|ui| {
            if programmes.len() > 1 {
                ui.label("Service");
                let label = |p: &Programme| {
                    format!(
                        "{} {}",
                        p.lcn.map_or(String::new(), |n| n.to_string()),
                        p.name.as_deref().unwrap_or("")
                    )
                };
                egui::ComboBox::from_id_salt("tv_service")
                    .selected_text(current.map(label).unwrap_or_default())
                    .show_ui(ui, |ui| {
                        for p in programmes {
                            if ui
                                .selectable_label(Some(p.number) == self.service, label(p))
                                .clicked()
                                && Some(p.number) != self.service
                            {
                                *req = Some(Request::Play(p.number, None, None));
                            }
                        }
                    });
            }
            if let Some(p) = current {
                let sounds: Vec<_> = p.streams.iter().filter(|e| e.is_audio()).collect();
                if sounds.len() > 1 {
                    ui.label("Sound");
                    let sel = sounds
                        .iter()
                        .find(|e| Some(e.pid) == self.audio)
                        .map(|e| e.describe())
                        .unwrap_or_else(|| "none".into());
                    egui::ComboBox::from_id_salt("tv_audio")
                        .selected_text(sel)
                        .show_ui(ui, |ui| {
                            for e in &sounds {
                                if ui
                                    .selectable_label(Some(e.pid) == self.audio, e.describe())
                                    .clicked()
                                    && Some(e.pid) != self.audio
                                {
                                    *req = Some(Request::Play(p.number, Some(e.pid), self.subs));
                                }
                            }
                        });
                }
                let subs: Vec<_> = subtitle_streams(p).collect();
                if !subs.is_empty() && Kind::of(p) != Kind::Radio {
                    ui.label("Subtitles");
                    let sel = subs
                        .iter()
                        .find(|e| Some(e.pid) == self.subs)
                        .map_or("off".to_string(), |e| e.describe());
                    egui::ComboBox::from_id_salt("tv_subs")
                        .selected_text(sel)
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(self.subs.is_none(), "off").clicked()
                                && self.subs.is_some()
                            {
                                *req = Some(Request::Play(p.number, self.audio, None));
                            }
                            for e in &subs {
                                if ui
                                    .selectable_label(Some(e.pid) == self.subs, e.describe())
                                    .clicked()
                                    && Some(e.pid) != self.subs
                                {
                                    *req = Some(Request::Play(p.number, self.audio, Some(e.pid)));
                                }
                            }
                        });
                }
            }
            let playing = self.player.is_some();
            if ui
                .button(if playing { "⏹ Stop" } else { "▶ Play" })
                .clicked()
            {
                if playing {
                    self.stop();
                } else if let Some(s) = self.service {
                    *req = Some(Request::Play(s, self.audio, self.subs));
                }
            }
            let restart = |me: &Self, req: &mut Option<Request>| {
                if me.player.is_some()
                    && let Some(s) = me.service
                {
                    *req = Some(Request::Play(s, me.audio, me.subs));
                }
            };
            if ui
                .checkbox(&mut self.bob, "Bob ×2")
                .on_hover_text(
                    "Deinterlace to a picture per field (50 i → 50 pictures a second); \
                     off: one per frame. Restarts the picture; remembered",
                )
                .changed()
            {
                prefs::set_bob(self.bob);
                restart(self, req);
            }
            if ui
                .checkbox(&mut self.gpu, "GPU decode")
                .on_hover_text(
                    "Decode the video on the graphics card (NVDEC, D3D11VA…), leaving the \
                     CPU to the receiver; falls back to software if the card cannot. \
                     Restarts the picture; remembered",
                )
                .changed()
            {
                prefs::set_gpu(self.gpu);
                restart(self, req);
            }
            ui.label("\u{1F50A}");
            let slider = ui
                .add(
                    egui::Slider::new(&mut self.volume, 0.0..=1.0)
                        .show_value(false)
                        .trailing_fill(true),
                )
                .on_hover_text("This window's volume; remembered");
            if slider.changed()
                && let Some(p) = &self.player
            {
                p.set_volume(self.volume);
            }
            if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                prefs::set_volume(self.volume);
            }
            if let Some(p) = &self.player {
                let st = p.status();
                let (col, text) = if st.running && (st.frames_shown > 0 || st.audio.is_some()) {
                    (
                        Color32::from_rgb(110, 220, 110),
                        if self.picture {
                            format!("playing · timed by the {}", st.clock)
                        } else {
                            "playing".to_string()
                        },
                    )
                } else if st.running {
                    (
                        Color32::from_rgb(240, 200, 80),
                        "ffmpeg starting…".to_string(),
                    )
                } else {
                    (
                        Color32::from_rgb(240, 200, 80),
                        st.exit.unwrap_or_else(|| "restarting ffmpeg…".into()),
                    )
                };
                ui.colored_label(col, text);
            }
        });
        if let Some(e) = &self.error {
            ui.colored_label(Color32::from_rgb(230, 110, 110), e);
            ui.horizontal(|ui| {
                ui.label("ffmpeg");
                if self.ffmpeg_edit.is_empty() {
                    self.ffmpeg_edit = prefs::ffmpeg_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                }
                ui.add(
                    egui::TextEdit::singleline(&mut self.ffmpeg_edit)
                        .desired_width(360.0)
                        .hint_text(r"C:\ffmpeg\bin\ffmpeg.exe"),
                );
                if ui.button("Browse…").clicked()
                    && let Some(p) = rfd::FileDialog::new()
                        .add_filter("ffmpeg", &["exe"])
                        .pick_file()
                {
                    self.ffmpeg_edit = p.display().to_string();
                }
                if ui.button("Use").clicked() {
                    let p = PathBuf::from(self.ffmpeg_edit.trim());
                    prefs::set_ffmpeg_path(Some(&p));
                    if let Some(s) = self.service {
                        *req = Some(Request::Play(s, self.audio, self.subs));
                    }
                }
            });
        }
    }

    /// The picture, as large as the window allows at 16:9.
    fn picture(&self, ui: &mut Ui) {
        let avail = ui.available_size() - vec2(0.0, 56.0);
        let w = avail.x.min(avail.y.max(120.0) * 16.0 / 9.0).max(160.0);
        let (rect, _) = ui.allocate_exact_size(vec2(w, w * 9.0 / 16.0), Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, 0.0, Color32::BLACK);
        match &self.texture {
            Some(t) if self.player.is_some() && self.picture => {
                p.image(
                    t.id(),
                    self.red.video_rect(rect),
                    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            _ if self.red.active() => {}
            _ => {
                p.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "no picture",
                    egui::FontId::proportional(16.0),
                    Color32::GRAY,
                );
            }
        }
        self.red.paint(p, rect);
    }

    /// A radio service: its name, what is on and the levels of the sound.
    fn radio(&self, ui: &mut Ui, current: Option<&Programme>) {
        let h = 150.0;
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::same(4), Color32::from_rgb(10, 10, 14));
        let name = current.and_then(|c| c.name.as_deref()).unwrap_or("radio");
        p.text(
            rect.left_top() + vec2(16.0, 14.0),
            egui::Align2::LEFT_TOP,
            name,
            egui::FontId::proportional(24.0),
            Color32::WHITE,
        );
        if let Some(now) = current.and_then(|c| c.now.as_ref()) {
            p.text(
                rect.left_top() + vec2(16.0, 48.0),
                egui::Align2::LEFT_TOP,
                &now.name,
                egui::FontId::proportional(15.0),
                Color32::LIGHT_GRAY,
            );
        }
        // Peak levels, −60 dB to 0, left over right.
        let levels = self
            .player
            .as_ref()
            .and_then(|p| p.levels())
            .unwrap_or([0.0, 0.0]);
        for (i, l) in levels.iter().enumerate() {
            let db = 20.0 * l.max(1e-4).log10();
            let f = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
            let bar = Rect::from_min_size(
                rect.left_top() + vec2(16.0, 86.0 + 20.0 * i as f32),
                vec2((rect.width() - 32.0) * f, 12.0),
            );
            let col = if db > -3.0 {
                Color32::from_rgb(255, 90, 80)
            } else {
                Color32::from_rgb(110, 220, 130)
            };
            p.rect_filled(bar, CornerRadius::same(2), col);
        }
    }

    /// What ffmpeg says the streams are, and how playing goes.
    fn details(&self, ui: &mut Ui) {
        let Some(p) = &self.player else { return };
        let st = p.status();
        // The streams ffmpeg found; the ones with nothing it can decode (data
        // carousels of a red button service) are not worth a line.
        for (pid, d) in &st.streams {
            if d.starts_with("Unknown") || d.starts_with("Data:") {
                continue;
            }
            let pid = pid.map_or(String::new(), |p| format!("{p:#06x}  "));
            ui.label(RichText::new(format!("{pid}{d}")).small().weak());
        }
        let mut bits = Vec::new();
        if self.picture {
            bits.push(format!(
                "{} pictures shown, {} dropped",
                st.frames_shown, st.frames_dropped
            ));
        }
        if st.frames_skipped > 0 {
            bits.push(format!("{} skipped to catch up", st.frames_skipped));
        }
        if self.tap_dropped > 0 {
            bits.push(format!(
                "{} packets lost before the player (it fell behind)",
                self.tap_dropped
            ));
        }
        if let Some(a) = &st.audio {
            bits.push(format!("sound buffered {} ms", a.buffer_ms));
            if a.underruns > 0 {
                bits.push(format!("{} underruns", a.underruns));
            }
        }
        if st.errors > 0 {
            bits.push(format!("{} decoder complaints", st.errors));
        }
        if st.starts > 1 {
            bits.push(format!("ffmpeg started {} times", st.starts));
        }
        ui.label(RichText::new(bits.join("  ·  ")).small());
        if let Some(e) = &st.last_error {
            ui.label(RichText::new(format!("last: {e}")).small().weak());
        }
    }
}

/// The video PID of a service, for the tap's backlog.
pub fn video_pid(p: &Programme) -> Option<u16> {
    p.streams.iter().find(|e| e.is_video()).map(|e| e.pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_ts::EsInfo;

    #[test]
    fn a_service_needs_its_tables_and_streams() {
        let mut sub = EsInfo::new(0x24, 0x06);
        sub.kind = Some("subtitles");
        let p = Programme {
            number: 3,
            pmt_pid: 0x20,
            pcr_pid: Some(0x21),
            streams: vec![
                EsInfo::new(0x21, 0x1B),
                EsInfo::new(0x22, 0x03),
                EsInfo::new(0x23, 0x03),
                sub,
            ],
            ..Default::default()
        };
        let s = selection(&p, None, None);
        // The other sound and the subtitles are left out unless chosen.
        assert_eq!(s.pids, [0, 0x20, 0x21, 0x22]);
        assert_eq!((s.video, s.audio, s.subs), (Some(0x21), Some(0x22), None));
        let s = selection(&p, Some(0x23), Some(0x24));
        assert_eq!((s.audio, s.subs), (Some(0x23), Some(0x24)));
        assert_eq!(s.pids, [0, 0x20, 0x21, 0x23, 0x24]);
        // Not streams of it: the first sound, no subtitles.
        let s = selection(&p, Some(0x99), Some(0x98));
        assert_eq!((s.audio, s.subs), (Some(0x22), None));
    }
}
