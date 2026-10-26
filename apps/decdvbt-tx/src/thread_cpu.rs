// made by CasualArclamp
//! CPU time of the calling thread, to tell what the window itself costs (its
//! layout, tessellation and OpenGL calls all run on the UI thread) from what
//! the transmit pipeline costs. Windows only; elsewhere it reads 0.

/// Seconds of CPU (user + kernel) this thread has used so far.
#[cfg(windows)]
pub fn seconds() -> f64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the pseudo-handle of the current thread is always valid and the
    // four out-parameters are live FILETIMEs.
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return 0.0;
    }
    // FILETIME counts 100 ns ticks.
    let ticks = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64;
    (ticks(kernel) + ticks(user)) * 1e-7
}

#[cfg(not(windows))]
pub fn seconds() -> f64 {
    0.0
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_counts_work_done_on_this_thread() {
        let t0 = super::seconds();
        // Spin for 200 ms of wall time (the OS counts thread time in ticks of
        // about 15 ms, so it needs a good stretch to show).
        let start = std::time::Instant::now();
        let mut x = 0u64;
        while start.elapsed().as_millis() < 200 {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        let dt = super::seconds() - t0;
        if cfg!(windows) {
            assert!(dt > 0.05, "{dt}");
        }
    }
}
