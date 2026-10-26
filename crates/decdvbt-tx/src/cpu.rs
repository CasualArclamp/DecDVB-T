// made by CasualArclamp
//! The process's CPU time, for the "CPU %" meter (100 % is one core).
//!
//! Copied from `decdvbt-engine/src/meter.rs` so this crate does not pull in
//! the whole receiver (`decdvbt-engine` depends on `decdvbt-rx`).

use std::time::Instant;

/// CPU seconds (user + kernel) this process has used so far.
#[cfg(windows)]
pub fn process_cpu_seconds() -> f64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no
    // closing, and the four pointers are to live locals the call fills in.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return 0.0;
    }
    let ticks = |t: FILETIME| (((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64) as f64;
    (ticks(kernel) + ticks(user)) * 1e-7
}

/// CPU seconds (user + kernel) this process has used so far.
#[cfg(not(windows))]
pub fn process_cpu_seconds() -> f64 {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return 0.0;
    };
    let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
        return 0.0;
    };
    let f: Vec<&str> = rest.split_whitespace().collect();
    let tick = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    (tick(11) + tick(12)) / 100.0
}

/// Turns the running CPU counter into a percentage over each interval.
/// This process only: ffmpeg's CPU is a separate process and is not in it
/// (the feeder reports it on its own, see `FeederStatus::ffmpeg_cpu_pct`).
pub struct CpuGauge {
    last_cpu: f64,
    last_at: Instant,
    pct: f32,
}

impl CpuGauge {
    pub fn new() -> CpuGauge {
        CpuGauge {
            last_cpu: process_cpu_seconds(),
            last_at: Instant::now(),
            pct: 0.0,
        }
    }

    /// Call often; the reading is renewed about once a second.
    pub fn update(&mut self) -> f32 {
        let dt = self.last_at.elapsed().as_secs_f64();
        if dt >= 1.0 {
            let cpu = process_cpu_seconds();
            self.pct = (100.0 * (cpu - self.last_cpu) / dt) as f32;
            self.last_cpu = cpu;
            self.last_at = Instant::now();
        }
        self.pct
    }
}

impl Default for CpuGauge {
    fn default() -> Self {
        CpuGauge::new()
    }
}
