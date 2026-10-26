// made by CasualArclamp
//! Draws the app's icon: `cargo run -p decdvbt-tx-app --example make_icon`
//! writes `assets/icon.ico` (16 to 256 px, PNG inside) and `assets/icon-256.png`
//! (the window icon, compiled into the app).
//!
//! The receiver's icon (a rounded square with a 4 x 4 grid of dots, a 16-QAM
//! constellation) in the transmitter's colours: a dark red square with a red
//! rim, pale dots and the four corner points in red. Each size is drawn from the
//! shapes' distance functions (not scaled down from the big one), so the edges
//! are as sharp at 16 px as at 256.

use std::io::Cursor;

use image::{ImageFormat, Rgba, RgbaImage};

const SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

const PALE: [f32; 3] = [244.0, 226.0, 220.0];
const RED: [f32; 3] = [255.0, 84.0, 74.0];
const TOP: [f32; 3] = [84.0, 22.0, 30.0];
const BOTTOM: [f32; 3] = [30.0, 7.0, 12.0];
const RIM: [f32; 3] = [196.0, 74.0, 70.0];

/// Coverage of a shape whose signed distance (in pixels, negative inside) is
/// `d`: one pixel of anti-aliasing across the edge.
fn cover(d: f32) -> f32 {
    (0.5 - d).clamp(0.0, 1.0)
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn render(size: u32) -> RgbaImage {
    let s = size as f32;
    let mut img = RgbaImage::new(size, size);
    // The square: half-side and corner radius, as fractions of the size.
    let (half, radius) = (0.47, 0.21);
    // The dots: a 4 x 4 grid, but 3 x 3 and bolder where there are too few
    // pixels for sixteen to read (the 16 and 24 px icons).
    let n: usize = if size < 32 { 3 } else { 4 };
    let spacing = if n == 3 { 0.235 } else { 0.175 };
    let dot_r = 0.056f32.max(if n == 3 { 1.35 } else { 1.05 } / s);
    let mid = (n as f32 - 1.0) / 2.0;
    for (x, y, px) in img.enumerate_pixels_mut() {
        let (u, v) = ((x as f32 + 0.5) / s, (y as f32 + 0.5) / s);
        // Signed distance to the rounded square, in pixels.
        let (qx, qy) = (
            (u - 0.5).abs() - (half - radius),
            (v - 0.5).abs() - (half - radius),
        );
        let sd = ((qx.max(0.0)).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius) * s;
        let alpha = cover(sd);
        if alpha <= 0.0 {
            *px = Rgba([0, 0, 0, 0]);
            continue;
        }
        // Diagonal gradient, a thin lighter rim just inside the edge.
        let mut c = mix(TOP, BOTTOM, ((u + v) * 0.5).clamp(0.0, 1.0));
        let rim = (1.0 - (-sd / (0.018 * s).max(1.0)).clamp(0.0, 1.0)) * 0.7;
        c = mix(c, RIM, rim * cover(-sd - 0.0).max(0.0));
        // The dots, with a soft glow where there is room for one.
        for i in 0..n {
            for j in 0..n {
                let (cx, cy) = (
                    0.5 + (i as f32 - mid) * spacing,
                    0.5 + (j as f32 - mid) * spacing,
                );
                let d = ((u - cx).hypot(v - cy) - dot_r) * s;
                let corner = (i == 0 || i == n - 1) && (j == 0 || j == n - 1);
                let col = if corner { RED } else { PALE };
                if size >= 48 {
                    let g = (-((u - cx).hypot(v - cy) / 0.05).powi(2)).exp() * 0.22;
                    c = mix(c, col, g);
                }
                c = mix(c, col, cover(d));
            }
        }
        *px = Rgba([
            c[0].round() as u8,
            c[1].round() as u8,
            c[2].round() as u8,
            (alpha * 255.0).round() as u8,
        ]);
    }
    img
}

fn png(img: &RgbaImage) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).expect("PNG");
    out.into_inner()
}

/// An .ico holding each image as a PNG (Windows Vista and later).
fn ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![0, 0, 1, 0];
    out.extend((images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, data) in images {
        // A width of 0 means 256.
        let b = if *size >= 256 { 0 } else { *size as u8 };
        out.extend([b, b, 0, 0, 1, 0, 32, 0]);
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in images {
        out.extend(data);
    }
    out
}

fn main() -> std::io::Result<()> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    std::fs::create_dir_all(&dir)?;
    let images: Vec<(u32, Vec<u8>)> = SIZES.iter().map(|&s| (s, png(&render(s)))).collect();
    std::fs::write(dir.join("icon.ico"), ico(&images))?;
    let big = &images.last().expect("sizes").1;
    std::fs::write(dir.join("icon-256.png"), big)?;
    // A contact sheet for a look: every size on a grey ground.
    let mut sheet = RgbaImage::from_pixel(680, 272, Rgba([58, 58, 62, 255]));
    let mut x = 8;
    for s in [256, 128, 64, 48, 32, 24, 16] {
        image::imageops::overlay(&mut sheet, &render(s), x as i64, 8);
        x += s as i32 + 12;
    }
    let out = dir.join("..").join("..").join("..").join("out");
    std::fs::create_dir_all(&out)?;
    sheet.save(out.join("icon-tx-preview.png")).ok();
    println!("wrote {}", dir.display());
    Ok(())
}
