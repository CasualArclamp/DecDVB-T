// made by CasualArclamp
//! What the app remembers between runs: a few `key=value` lines in
//! `%APPDATA%\DecDVB-T\prefs.txt` (`~/.config/decdvbt/prefs.txt` elsewhere).
//! Nothing here is needed to run: a file that cannot be read or written is
//! the same as an empty one.

use std::path::PathBuf;

fn file() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("DecDVB-T"))
    } else {
        std::env::var_os("HOME").map(|d| PathBuf::from(d).join(".config").join("decdvbt"))
    }?;
    Some(base.join("prefs.txt"))
}

pub fn get(key: &str) -> Option<String> {
    let text = std::fs::read_to_string(file()?).ok()?;
    text.lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.trim().to_string())
}

pub fn set(key: &str, value: &str) {
    let Some(path) = file() else { return };
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = old
        .lines()
        .filter(|l| l.split_once('=').is_none_or(|(k, _)| k != key))
        .map(String::from)
        .collect();
    if !value.is_empty() {
        lines.push(format!("{key}={value}"));
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, lines.join("\n") + "\n");
}

pub fn ffmpeg_path() -> Option<PathBuf> {
    get("ffmpeg").filter(|s| !s.is_empty()).map(PathBuf::from)
}

pub fn set_ffmpeg_path(p: Option<&std::path::Path>) {
    set(
        "ffmpeg",
        &p.map_or(String::new(), |p| p.display().to_string()),
    );
}

pub fn volume() -> f32 {
    get("volume")
        .and_then(|v| v.parse().ok())
        .map_or(0.8, |v: f32| v.clamp(0.0, 1.0))
}

pub fn set_volume(v: f32) {
    set("volume", &format!("{v:.2}"));
}

fn flag(key: &str, default: bool) -> bool {
    get(key).map_or(default, |v| v == "1")
}

pub fn bob() -> bool {
    flag("bob", true)
}

pub fn set_bob(on: bool) {
    set("bob", if on { "1" } else { "0" });
}

pub fn gpu() -> bool {
    flag("gpu", false)
}

pub fn set_gpu(on: bool) {
    set("gpu", if on { "1" } else { "0" });
}
