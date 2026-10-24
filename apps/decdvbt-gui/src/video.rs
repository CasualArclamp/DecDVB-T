// made by CasualArclamp
//! Recording the window to a video file, for demos: `--video OUT.mp4
//! --video-secs N` with `--screenshot` and `--after` (the video starts at
//! `--after` and the screenshot is its last frame). Each frame the GUI draws
//! is read back and piped raw to ffmpeg, which encodes it; frames are
//! repeated or skipped to keep 30 a second of wall time, so the video plays
//! at the speed the window ran. Nothing of the desktop, the mouse or other
//! windows gets in, unlike a screen grab.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Instant;

use eframe::egui::ColorImage;

/// Frames a second of the video.
pub const FPS: f64 = 30.0;

pub struct VideoTap {
    child: Child,
    stdin: Option<ChildStdin>,
    size: [usize; 2],
    start: Instant,
    written: u64,
    rgba: Vec<u8>,
}

impl VideoTap {
    /// ffmpeg encoding raw frames of `size` to `out` (H.264).
    pub fn start(ffmpeg: &Path, out: &PathBuf, size: [usize; 2]) -> std::io::Result<VideoTap> {
        let mut child = Command::new(ffmpeg)
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-s",
            ])
            .arg(format!("{}x{}", size[0], size[1]))
            .args(["-r", "30", "-i", "-", "-c:v", "libx264", "-preset", "fast"])
            .args(["-crf", "16", "-pix_fmt", "yuv420p"])
            .arg(out)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take();
        Ok(VideoTap {
            child,
            stdin,
            size,
            start: Instant::now(),
            written: 0,
            rgba: Vec::new(),
        })
    }

    /// Seconds recorded so far.
    pub fn elapsed(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    /// A frame the GUI drew: written as many times as the video's clock has
    /// moved on since the last (none if it has not; a frame of another size,
    /// a resized window, is left out).
    pub fn push(&mut self, image: &ColorImage) -> std::io::Result<()> {
        if image.size != self.size {
            return Ok(());
        }
        let due = (self.elapsed() * FPS) as u64 + 1;
        if due <= self.written {
            return Ok(());
        }
        self.rgba.clear();
        self.rgba
            .extend(image.pixels.iter().flat_map(|c| c.to_array()));
        if let Some(s) = &mut self.stdin {
            while self.written < due {
                s.write_all(&self.rgba)?;
                self.written += 1;
            }
        }
        Ok(())
    }

    /// Close the pipe and wait for ffmpeg to finish the file.
    pub fn finish(mut self) -> std::io::Result<u64> {
        drop(self.stdin.take());
        self.child.wait()?;
        Ok(self.written)
    }
}
