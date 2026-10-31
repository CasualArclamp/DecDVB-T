// made by CasualArclamp
//! Drawing the engine's visibles on a 720x576 RGBA canvas (the UK profile's
//! graphics plane, D-Book 7 §13.10): rectangles, PNG bitmaps, text, and
//! video as a clear hole the picture shows through. Pixels nothing covers
//! are clear too.
//!
//! Text: the font attributes "style.size.lineSpacing.letterSpacing"
//! (D-Book 7 §13.9.3; size and line spacing in pixels, letter spacing in
//! 1/256 of a pixel), the text's own markup (ESC 'C' with four bytes RGBT
//! sets the colour, ESC 'c' restores it; a control byte 0x40–0x5E takes a
//! length and its data, 0x60–0x7E none; CR breaks the line; tab steps to the
//! next 45-pixel stop), wrapping at spaces, and the four justifications. The
//! receiver's font is Tiresias Screenfont; this engine uses the closest font
//! the machine has (`DECDVBT_MHEG_FONT`, else Tiresias if installed, else
//! Verdana, else DejaVu Sans), so line lengths differ a little. Layout after
//! freemheg's `Text.cpp` (GPL-2.0-or-later).
//!
//! Cost: the canvas is drawn only when the engine says something changed,
//! and glyphs are rasterised once per size.

use std::collections::HashMap;
use std::rc::Rc;

use ab_glyph::{Font, FontVec, GlyphId, PxScale, ScaleFont};

use crate::engine::Engine;
use crate::obj::*;
use crate::tags::C_BITMAP;

pub const WIDTH: usize = 720;
pub const HEIGHT: usize = 576;
/// Tab stops (freemheg's, D-Book 7 §13.9.2).
const TAB: i32 = 45;

/// A bitmap's bytes as RGBA: PNG (the UK's bitmap format, content hook 4).
/// MPEG-2 I-frames (hook 2) are not decoded here.
pub fn decode_bitmap(b: &[u8]) -> Option<Image> {
    if !b.starts_with(b"\x89PNG") {
        return None;
    }
    let mut d = png::Decoder::new(std::io::Cursor::new(b));
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = d.read_info().ok()?;
    let mut buf = vec![0; r.output_buffer_size()?];
    let info = r.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let px = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.chunks(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => px
            .chunks(2)
            .flat_map(|c| [c[0], c[0], c[0], c[1]])
            .collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&c| [c, c, c, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    Some(Image { w, h, rgba })
}

/// A rasterised glyph: coverage, and its offset from the pen.
struct Glyph {
    w: i32,
    h: i32,
    left: i32,
    top: i32,
    cover: Vec<u8>,
}

/// A bitmap scaled down into its box: the image it came from (to tell a
/// reused address) and the small copy.
type Shrunk = (std::rc::Weak<Image>, Rc<Image>);

/// The canvas and what draws on it.
pub struct Renderer {
    pub rgba: Vec<u8>,
    font: Option<FontVec>,
    pub font_name: String,
    /// The font's horizontal scale (see [`font_candidates`]).
    squeeze: f32,
    glyphs: HashMap<(GlyphId, u32, u32), Rc<Glyph>>,
    /// Bitmaps scaled down into their boxes: (image's address, w, h) ->
    /// (the image, to tell a reused address, and the small copy).
    shrunk: HashMap<(usize, u32, u32), Shrunk>,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

/// The font files tried, in order, each with the horizontal squeeze that
/// brings its line lengths to about Tiresias Screenfont's (measured on the
/// BBC's pages: "said she was informed about", a line it breaks after, fills
/// its 262-pixel box at 24 pixels).
fn font_candidates() -> Vec<(String, f32)> {
    let mut v = Vec::new();
    if let Ok(f) = std::env::var("DECDVBT_MHEG_FONT") {
        v.push((f, 1.0));
    }
    let win = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    for (f, x) in [
        ("Tiresias Screenfont.ttf", 1.0),
        ("TiresiasScreenfont.ttf", 1.0),
        ("segoeui.ttf", 0.85),
        ("arial.ttf", 0.84),
        ("verdana.ttf", 0.72),
    ] {
        v.push((format!("{win}\\Fonts\\{f}"), x));
    }
    for (f, x) in [
        (
            "/usr/share/fonts/truetype/tiresias/TiresiasScreenfont.ttf",
            1.0,
        ),
        ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 0.72),
        ("/usr/share/fonts/TTF/DejaVuSans.ttf", 0.72),
        ("/Library/Fonts/Verdana.ttf", 0.72),
    ] {
        v.push((f.into(), x));
    }
    v
}

impl Renderer {
    pub fn new() -> Self {
        let mut font = None;
        let mut font_name = String::from("none");
        let mut squeeze = 1.0;
        for (f, x) in font_candidates() {
            if let Ok(b) = std::fs::read(&f)
                && let Ok(fv) = FontVec::try_from_vec(b)
            {
                font = Some(fv);
                font_name = f;
                squeeze = x;
                break;
            }
        }
        Renderer {
            rgba: vec![0; WIDTH * HEIGHT * 4],
            font,
            font_name,
            squeeze,
            glyphs: HashMap::new(),
            shrunk: HashMap::new(),
        }
    }

    /// A line's width in pixels at a size, as laid out here.
    pub fn text_width(&self, text: &str, size: f32) -> f32 {
        let Some(font) = self.font.as_ref() else {
            return 0.0;
        };
        let sf = font.as_scaled(px_scale(font, size, self.squeeze));
        text.chars().map(|c| sf.h_advance(sf.glyph_id(c))).sum()
    }

    /// Draw the engine's scene (and application) visibles.
    pub fn render(&mut self, e: &Engine) {
        self.rgba.fill(0);
        let defaults = e
            .app()
            .and_then(|a| {
                e.objs.get(&Ref {
                    group: a.into(),
                    num: 0,
                })
            })
            .and_then(|o| match &o.kind {
                Kind::Group(g) => Some(g.default.clone()),
                _ => None,
            })
            .unwrap_or_default();
        for r in &e.display {
            let Some(o) = e.objs.get(r) else { continue };
            if !o.running {
                continue;
            }
            let Kind::Visible(v) = &o.kind else { continue };
            self.visible(o, v, &defaults);
        }
    }

    fn visible(&mut self, o: &Obj, v: &Visible, d: &Defaults) {
        let (x, y) = v.pos;
        let (w, h) = v.size;
        match &v.draw {
            Draw::Rectangle => {
                let lw = v.line_width.max(0);
                self.fill(x, y, w, h, v.fill_colour);
                if lw > 0 && v.line_colour.t != 255 {
                    self.fill(x, y, w, lw, v.line_colour);
                    self.fill(x, y + h - lw, w, lw, v.line_colour);
                    self.fill(x, y + lw, lw, h - 2 * lw, v.line_colour);
                    self.fill(x + w - lw, y + lw, lw, h - 2 * lw, v.line_colour);
                }
            }
            Draw::Bitmap {
                tiling,
                image,
                scale,
                offset,
            } => {
                if let Some(img) = image {
                    // A bitmap larger than its box is scaled into it, not
                    // clipped (MythTV's mhi.cpp: "the BBC expects bitmaps
                    // to be scaled"; Freeview's internet channels send their
                    // titles and logos at the HD plane's size, 500x70 for a
                    // 282x56 box): drawn in the part of the box it covers.
                    let fit = (img.w as i32).min(w).max(1) as u32;
                    let fit_h = (img.h as i32).min(h).max(1) as u32;
                    if !*tiling
                        && scale.is_none()
                        && *offset == (0, 0)
                        && (fit, fit_h) != (img.w, img.h)
                    {
                        let small = self.shrunk(img, fit, fit_h);
                        self.bitmap(&small, x, y, w, h, false, None, (0, 0));
                    } else {
                        self.bitmap(img, x, y, w, h, *tiling, *scale, *offset);
                    }
                }
                let _ = o.class == C_BITMAP;
            }
            Draw::Text {
                font_attributes,
                h_just,
                v_just,
                wrap,
            } => {
                self.fill(x, y, w, h, v.background.unwrap_or(d.background));
                let attrs = if font_attributes.is_empty() {
                    &d.font_attributes
                } else {
                    font_attributes
                };
                let colour = v.text_colour.unwrap_or(d.text_colour);
                self.text(&o.data, x, y, w, h, attrs, colour, *h_just, *v_just, *wrap);
            }
            Draw::Video { .. } => self.clear(x, y, w, h),
            Draw::Other => {}
        }
    }

    fn clip(x: i32, y: i32, w: i32, h: i32) -> Option<(usize, usize, usize, usize)> {
        let (x0, y0) = (x.max(0), y.max(0));
        let (x1, y1) = ((x + w).min(WIDTH as i32), (y + h).min(HEIGHT as i32));
        (x1 > x0 && y1 > y0).then_some((x0 as usize, y0 as usize, x1 as usize, y1 as usize))
    }

    fn clear(&mut self, x: i32, y: i32, w: i32, h: i32) {
        if let Some((x0, y0, x1, y1)) = Self::clip(x, y, w, h) {
            for row in y0..y1 {
                self.rgba[(row * WIDTH + x0) * 4..(row * WIDTH + x1) * 4].fill(0);
            }
        }
    }

    /// Paint `c` with coverage `a` (0–255) over the pixel at index `i`.
    #[inline]
    fn blend(&mut self, i: usize, c: [u8; 3], a: u32) {
        if a == 0 {
            return;
        }
        let p = &mut self.rgba[i..i + 4];
        let da = p[3] as u32;
        let oa = a + da * (255 - a) / 255;
        if oa == 0 {
            return;
        }
        for k in 0..3 {
            p[k] = ((c[k] as u32 * a + p[k] as u32 * da * (255 - a) / 255) / oa) as u8;
        }
        p[3] = oa as u8;
    }

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, c: Colour) {
        let a = 255 - c.t as u32;
        if a == 0 {
            return;
        }
        if let Some((x0, y0, x1, y1)) = Self::clip(x, y, w, h) {
            for row in y0..y1 {
                for col in x0..x1 {
                    self.blend((row * WIDTH + col) * 4, [c.r, c.g, c.b], a);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn bitmap(
        &mut self,
        img: &Image,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        tiling: bool,
        scale: Option<(i32, i32)>,
        offset: (i32, i32),
    ) {
        let (sw, sh) = scale.map_or((img.w as i32, img.h as i32), |s| (s.0.max(1), s.1.max(1)));
        let Some((x0, y0, x1, y1)) = Self::clip(x, y, w, h) else {
            return;
        };
        for row in y0..y1 {
            let mut iy = row as i32 - y - offset.1;
            if tiling {
                iy = iy.rem_euclid(sh);
            } else if iy < 0 || iy >= sh {
                continue;
            }
            let sy = (iy as i64 * img.h as i64 / sh as i64) as usize;
            for col in x0..x1 {
                let mut ix = col as i32 - x - offset.0;
                if tiling {
                    ix = ix.rem_euclid(sw);
                } else if ix < 0 || ix >= sw {
                    continue;
                }
                let sx = (ix as i64 * img.w as i64 / sw as i64) as usize;
                let s = (sy * img.w as usize + sx) * 4;
                let p = &img.rgba[s..s + 4];
                self.blend((row * WIDTH + col) * 4, [p[0], p[1], p[2]], p[3] as u32);
            }
        }
    }

    /// `img` made `w` x `h` by averaging the pixels each new one covers
    /// (alpha-weighted, so a white title on clear stays white), kept until
    /// the image goes.
    fn shrunk(&mut self, img: &Rc<Image>, w: u32, h: u32) -> Rc<Image> {
        let key = (Rc::as_ptr(img) as usize, w, h);
        if let Some((weak, small)) = self.shrunk.get(&key)
            && weak.upgrade().is_some_and(|i| Rc::ptr_eq(&i, img))
        {
            return small.clone();
        }
        // Images that went are dropped from the cache now and then.
        self.shrunk.retain(|_, (weak, _)| weak.strong_count() > 0);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for dy in 0..h {
            let (y0, y1) = (
                dy * img.h / h,
                ((dy + 1) * img.h / h).max(dy * img.h / h + 1),
            );
            for dx in 0..w {
                let (x0, x1) = (
                    dx * img.w / w,
                    ((dx + 1) * img.w / w).max(dx * img.w / w + 1),
                );
                let (mut acc, mut alpha, mut n) = ([0u64; 3], 0u64, 0u64);
                for sy in y0..y1.min(img.h) {
                    for sx in x0..x1.min(img.w) {
                        let p = &img.rgba[((sy * img.w + sx) * 4) as usize..][..4];
                        let a = p[3] as u64;
                        for k in 0..3 {
                            acc[k] += p[k] as u64 * a;
                        }
                        alpha += a;
                        n += 1;
                    }
                }
                let o = ((dy * w + dx) * 4) as usize;
                for k in 0..3 {
                    rgba[o + k] = acc[k].checked_div(alpha).unwrap_or(0) as u8;
                }
                rgba[o + 3] = (alpha / n.max(1)) as u8;
            }
        }
        let small = Rc::new(Image { w, h, rgba });
        self.shrunk.insert(key, (Rc::downgrade(img), small.clone()));
        small
    }

    /// A glyph at `size`, squeezed across by `k` on top of the font's own
    /// squeeze.
    fn glyph(&mut self, id: GlyphId, size: f32, k: f32) -> Option<Rc<Glyph>> {
        let key = (id, (size * 16.0) as u32, (k * 100.0) as u32);
        if let Some(g) = self.glyphs.get(&key) {
            return Some(g.clone());
        }
        let font = self.font.as_ref()?;
        let scale = px_scale(font, size, self.squeeze * k);
        let g = id.with_scale_and_position(scale, ab_glyph::point(0.0, 0.0));
        let glyph = match font.outline_glyph(g) {
            Some(og) => {
                let b = og.px_bounds();
                let (w, h) = (b.width() as i32, b.height() as i32);
                let mut cover = vec![0u8; (w * h).max(0) as usize];
                og.draw(|gx, gy, c| {
                    let i = gy as i32 * w + gx as i32;
                    if i >= 0 && (i as usize) < cover.len() {
                        cover[i as usize] = (c.clamp(0.0, 1.0) * 255.0) as u8;
                    }
                });
                Glyph {
                    w,
                    h,
                    left: b.min.x as i32,
                    top: b.min.y as i32,
                    cover,
                }
            }
            None => Glyph {
                w: 0,
                h: 0,
                left: 0,
                top: 0,
                cover: Vec::new(),
            },
        };
        let g = Rc::new(glyph);
        self.glyphs.insert(key, g.clone());
        Some(g)
    }

    /// Lay out and draw a text in its box.
    #[allow(clippy::too_many_arguments)]
    fn text(
        &mut self,
        data: &[u8],
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        attrs: &[u8],
        colour: Colour,
        h_just: i32,
        v_just: i32,
        wrap: bool,
    ) {
        let Some(font) = self.font.as_ref() else {
            return;
        };
        let fa = FontAttrs::parse(attrs);
        let sf = font.as_scaled(px_scale(font, fa.size, self.squeeze));
        let spacing = fa.letter_spacing as f32 / 256.0;
        let adv = |ch: char| sf.h_advance(sf.glyph_id(ch)) + spacing;
        let runs = runs(data, colour);
        let lines = layout(&runs, w as f32, wrap, &adv);
        let total = lines.len() as i32 * fa.line_spacing;
        let top = match v_just {
            2 => y + h - total,
            3 => y + (h - total) / 2,
            _ => y,
        };
        // The baseline sits the font's ascent below the line's top, with the
        // line spacing's extra split above and below.
        let ascent = sf.ascent();
        let lead = (fa.line_spacing as f32 - (sf.ascent() - sf.descent())) / 2.0;
        let lines: Vec<Line<GlyphId>> = lines
            .into_iter()
            .map(|l| Line {
                glyphs: l
                    .glyphs
                    .into_iter()
                    .map(|(p, ch, c)| (p, sf.glyph_id(ch), c))
                    .collect(),
                width: l.width,
                k: l.k,
            })
            .collect();
        let size = fa.size;
        for (n, line) in lines.iter().enumerate() {
            // Only whole lines are drawn (the first always).
            if n > 0 && top + (n as i32 + 1) * fa.line_spacing > y + h {
                break;
            }
            let lw = line.width * line.k;
            let left = match h_just {
                2 => x as f32 + w as f32 - lw,
                3 => x as f32 + (w as f32 - lw) / 2.0,
                _ => x as f32,
            };
            let base = top as f32 + n as f32 * fa.line_spacing as f32 + lead + ascent;
            for &(p, id, c) in &line.glyphs {
                let Some(g) = self.glyph(id, size, line.k) else {
                    continue;
                };
                let gx = (left + p * line.k).round() as i32 + g.left;
                let gy = base.round() as i32 + g.top;
                let a = 255 - c.t as u32;
                for row in 0..g.h {
                    let py = gy + row;
                    if py < y.max(0) || py >= (y + h).min(HEIGHT as i32) {
                        continue;
                    }
                    for col in 0..g.w {
                        let px = gx + col;
                        if px < x.max(0) || px >= (x + w).min(WIDTH as i32) {
                            continue;
                        }
                        let cov = g.cover[(row * g.w + col) as usize] as u32;
                        if cov > 0 {
                            let i = (py as usize * WIDTH + px as usize) * 4;
                            self.blend(i, [c.r, c.g, c.b], cov * a / 255);
                        }
                    }
                }
            }
        }
    }
}

/// A laid-out line: glyphs at their pen positions, its width, and the
/// squeeze it is drawn with.
struct Line<G> {
    glyphs: Vec<(f32, G, Colour)>,
    width: f32,
    k: f32,
}

/// How much a line written for Tiresias may be squeezed to keep it whole
/// rather than wrap it (broadcasters break their lines for Tiresias's
/// widths, which no other font matches exactly).
const MAX_SQUEEZE: f32 = 0.82;

/// Lay out the text: lines break at CR; a line too wide for the box is
/// squeezed (to [`MAX_SQUEEZE`]) if that makes it fit, else wrapped at
/// spaces (when wrapping) or squeezed as far as it goes and cut.
fn layout(
    runs: &[(char, Colour)],
    w: f32,
    wrap: bool,
    adv: &dyn Fn(char) -> f32,
) -> Vec<Line<char>> {
    let mut out = Vec::new();
    for hard in runs.split(|r| matches!(r.0, '\r' | '\n')) {
        let line = place(hard, adv);
        if line.width <= w {
            out.push(line);
        } else if line.width * MAX_SQUEEZE <= w || !wrap {
            let k = (w / line.width).max(MAX_SQUEEZE);
            out.push(Line { k, ..line });
        } else {
            // Greedy by words: as many as fit (squeezed if need be).
            let mut start = 0;
            while start < hard.len() {
                let mut stop = None;
                let mut end = start;
                while end <= hard.len() {
                    let next = hard[end..]
                        .iter()
                        .position(|r| r.0 == ' ')
                        .map_or(hard.len(), |e| end + e);
                    if stop.is_some() && place(&hard[start..next], adv).width * MAX_SQUEEZE > w {
                        break;
                    }
                    stop = Some(next);
                    end = next + 1;
                }
                let stop = stop.unwrap_or(hard.len());
                let mut l = place(&hard[start..stop], adv);
                if l.width > w {
                    l.k = (w / l.width).max(MAX_SQUEEZE);
                }
                out.push(l);
                start = stop + 1;
                // Spaces at a break go with it.
                while start < hard.len() && hard[start].0 == ' ' {
                    start += 1;
                }
            }
        }
    }
    out
}

/// One line's glyphs at their pen positions (tabs to the next 45 pixels).
fn place(chars: &[(char, Colour)], adv: &dyn Fn(char) -> f32) -> Line<char> {
    let mut pen = 0f32;
    let mut glyphs = Vec::with_capacity(chars.len());
    let mut width = 0f32;
    for &(ch, c) in chars {
        if ch == '\t' {
            pen = (((pen as i32) / TAB + 1) * TAB) as f32;
            continue;
        }
        glyphs.push((pen, ch, c));
        pen += adv(ch);
        if ch != ' ' {
            width = pen;
        }
    }
    Line {
        glyphs,
        width,
        k: 1.0,
    }
}

/// The pixel scale that makes the font's em `size` pixels high, squeezed
/// across by `squeeze`.
fn px_scale(font: &FontVec, size: f32, squeeze: f32) -> PxScale {
    let upem = font.units_per_em().unwrap_or(1000.0);
    let y = size * font.height_unscaled() / upem;
    PxScale { x: y * squeeze, y }
}

/// "plain.24.27.0" (D-Book 7 §13.9.3): style, size, line spacing, letter
/// spacing; the short form "24" or "plain.24" gives the rest as defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct FontAttrs {
    pub size: f32,
    pub line_spacing: i32,
    pub letter_spacing: i32,
}

impl FontAttrs {
    pub fn parse(a: &[u8]) -> FontAttrs {
        let s = String::from_utf8_lossy(a);
        let parts: Vec<&str> = s.split('.').collect();
        let num = |i: usize| parts.get(i).and_then(|p| p.trim().parse::<i32>().ok());
        let (size, ls, sp) = if parts.first().is_some_and(|p| p.parse::<i32>().is_err()) {
            (num(1), num(2), num(3))
        } else {
            (num(0), num(1), num(2))
        };
        let size = size.unwrap_or(24).clamp(6, 200);
        FontAttrs {
            size: size as f32,
            // 0 ("plain.36.0.0") means the font's own spacing.
            line_spacing: ls.filter(|&l| l > 0).unwrap_or(size * 9 / 8 + 1),
            letter_spacing: sp.unwrap_or(0),
        }
    }
}

/// The text's characters with their colours, the markup read.
fn runs(data: &[u8], colour: Colour) -> Vec<(char, Colour)> {
    let mut out = Vec::new();
    let mut c = colour;
    let mut i = 0;
    let mut text = Vec::new();
    let flush = |text: &mut Vec<u8>, out: &mut Vec<(char, Colour)>, c: Colour| {
        for ch in String::from_utf8_lossy(text).chars() {
            if ch == '\r' || ch == '\n' || ch == '\t' || !ch.is_control() {
                out.push((ch, c));
            }
        }
        text.clear();
    };
    while i < data.len() {
        let b = data[i];
        if b == 0x1B && i + 1 < data.len() {
            flush(&mut text, &mut out, c);
            let k = data[i + 1];
            if (0x40..=0x5E).contains(&k) {
                let n = data.get(i + 2).copied().unwrap_or(0) as usize;
                let body = data.get(i + 3..i + 3 + n).unwrap_or(&[]);
                if k == b'C' && n >= 4 {
                    c = Colour::from_bytes(body);
                }
                i += 3 + n;
            } else {
                if k == b'c' {
                    c = colour;
                }
                i += 2;
            }
            continue;
        }
        text.push(b);
        i += 1;
    }
    flush(&mut text, &mut out, c);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_attributes() {
        assert_eq!(
            FontAttrs::parse(b"plain.24.27.0"),
            FontAttrs {
                size: 24.0,
                line_spacing: 27,
                letter_spacing: 0
            }
        );
        assert_eq!(FontAttrs::parse(b"31").size, 31.0);
        assert_eq!(FontAttrs::parse(b"plain.36.0.0").line_spacing, 41);
    }

    #[test]
    fn colour_markup() {
        let r = runs(b"a\x1bC\x04\xff\0\0\0b\x1bcc", Colour::WHITE);
        assert_eq!(r.len(), 3);
        assert_eq!(
            r[1].1,
            Colour {
                r: 255,
                g: 0,
                b: 0,
                t: 0
            }
        );
        assert_eq!(r[2].1, Colour::WHITE);
    }
}
