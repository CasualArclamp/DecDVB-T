// made by CasualArclamp
//! CPU meters. CPU efficiency is this project's main requirement
//! (`docs/DESIGN.md` §0), so the cost is measured, not guessed:
//!
//! - [`process_cpu_seconds`]: the whole app's CPU time, for the status bar's
//!   "CPU %" (100 % is one core, as in Task Manager's per-process view times
//!   the core count, and as OnAir shows it).
//! - [`StageMeter`]: wall time spent in each stage of the receive thread,
//!   for the per-stage breakdown. A stage's wall time is its CPU cost when it
//!   does not block (the spectrum, the recorder).
//! - [`CpuGauge`]: turns two readings of a running counter into a percentage.

use std::time::{Duration, Instant};

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
    // FILETIME counts 100 ns ticks, split over two u32s.
    let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64) as f64;
    (ticks(kernel) + ticks(user)) * 1e-7
}

/// CPU seconds (user + kernel) this process has used so far.
#[cfg(not(windows))]
pub fn process_cpu_seconds() -> f64 {
    // /proc/self/stat fields 14 and 15 are utime and stime in clock ticks
    // (100 a second on every Linux we run on). Field 2, the command name,
    // may hold spaces, so count from after its closing parenthesis.
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return 0.0;
    };
    let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
        return 0.0;
    };
    let f: Vec<&str> = rest.split_whitespace().collect();
    let tick = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    // After the ")" the fields start at number 3, so utime is index 11.
    (tick(11) + tick(12)) / 100.0
}

/// A running counter (CPU seconds) read now and then, and the percentage
/// between readings: 100 = one core fully busy.
pub struct CpuGauge {
    at: Instant,
    cpu: f64,
    pub percent: f32,
}

impl Default for CpuGauge {
    fn default() -> Self {
        CpuGauge {
            at: Instant::now(),
            cpu: process_cpu_seconds(),
            percent: 0.0,
        }
    }
}

impl CpuGauge {
    /// Read the counter; updates `percent` if at least half a second has
    /// passed since the last reading (shorter is too noisy to show).
    pub fn poll(&mut self) -> f32 {
        let dt = self.at.elapsed().as_secs_f64();
        if dt >= 0.5 {
            let cpu = process_cpu_seconds();
            self.percent = (100.0 * (cpu - self.cpu) / dt) as f32;
            self.at = Instant::now();
            self.cpu = cpu;
        }
        self.percent
    }
}

/// The stages of the receive thread, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Waiting for, and converting, samples (blocks on a live source).
    Source,
    /// Writing the recording.
    Record,
    /// FFT segments for the waterfall.
    Spectrum,
}

impl Stage {
    pub const ALL: [Stage; 3] = [Stage::Source, Stage::Record, Stage::Spectrum];

    pub fn name(self) -> &'static str {
        match self {
            Stage::Source => "source",
            Stage::Record => "record",
            Stage::Spectrum => "spectrum",
        }
    }
}

/// Wall time per stage over a window, as a share of the window.
#[derive(Debug, Clone, Copy, Default)]
pub struct StageLoad {
    /// Percent of the window, per [`Stage::ALL`] entry.
    pub percent: [f32; 3],
}

/// Accumulates the time each stage takes and reports it every second.
pub struct StageMeter {
    busy: [Duration; 3],
    window_start: Instant,
    pub last: StageLoad,
}

impl Default for StageMeter {
    fn default() -> Self {
        StageMeter {
            busy: [Duration::ZERO; 3],
            window_start: Instant::now(),
            last: StageLoad::default(),
        }
    }
}

impl StageMeter {
    /// Run `f`, counting its time against `stage`.
    pub fn time<T>(&mut self, stage: Stage, f: impl FnOnce() -> T) -> T {
        let t0 = Instant::now();
        let out = f();
        self.busy[stage as usize] += t0.elapsed();
        out
    }

    /// Close the window if a second has passed; true when `last` is new.
    pub fn tick(&mut self) -> bool {
        let dt = self.window_start.elapsed();
        if dt < Duration::from_secs(1) {
            return false;
        }
        for (p, b) in self.last.percent.iter_mut().zip(&self.busy) {
            *p = (100.0 * b.as_secs_f64() / dt.as_secs_f64()) as f32;
        }
        self.busy = [Duration::ZERO; 3];
        self.window_start = Instant::now();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_time_is_counted_against_its_stage() {
        let mut m = StageMeter::default();
        m.time(Stage::Spectrum, || {
            std::thread::sleep(Duration::from_millis(30))
        });
        assert!(m.busy[Stage::Spectrum as usize] >= Duration::from_millis(30));
        assert_eq!(m.busy[Stage::Record as usize], Duration::ZERO);
    }

    #[test]
    fn process_cpu_time_moves_when_work_is_done() {
        let before = process_cpu_seconds();
        let t0 = Instant::now();
        let mut x = 0u64;
        while t0.elapsed() < Duration::from_millis(150) {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(x);
        assert!(process_cpu_seconds() > before);
    }
}
