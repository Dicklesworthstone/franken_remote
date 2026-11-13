//! A clock that keeps counting across sleep and hibernation and that no one
//! can set: the Windows counterpart of Linux's `CLOCK_BOOTTIME`, for evidence
//! that must expire while the machine sleeps. And a one-millisecond system
//! timer for a process whose waits must keep a frame cadence.
use windows::Win32::{
    Media::{TIMERR_NOERROR, timeBeginPeriod, timeEndPeriod},
    System::{
        Threading::{
            GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
            ProcessPowerThrottling, SetProcessInformation,
        },
        WindowsProgramming::QueryInterruptTimePrecise,
    },
};

/// Nanoseconds since boot, sleep included (interrupt time, which is biased
/// by suspended time; the "unbiased" variant leaves it out).
pub fn since_boot_ns() -> u64 {
    // SAFETY: a counter read with no arguments; it cannot fail.
    let ticks = unsafe { QueryInterruptTimePrecise() };
    // 100 ns units; u64 nanoseconds last 584 years of uptime.
    ticks.saturating_mul(100)
}

/// A one-millisecond system timer for as long as the value lives.
///
/// Windows wakes a process's timed waits on the system timer tick, 15.625 ms
/// unless the process asks for less, so a wait for 16.7 ms (one frame at
/// 60/s) can last up to 31.25 ms. Windows 11 also ignores the request from a
/// process that shows no window unless the process opts out of that
/// throttling, which `begin` does first.
#[derive(Debug)]
pub struct FineTimer {
    began: bool,
}
impl FineTimer {
    pub fn begin() -> Self {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        let size = u32::try_from(size_of::<PROCESS_POWER_THROTTLING_STATE>()).unwrap_or(u32::MAX);
        // SAFETY: the current process's pseudo-handle and a valid state
        // structure of the stated size; a failure leaves the throttling as it
        // was (the timer may then be ignored while no window shows).
        let _ = unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                (&raw const state).cast(),
                size,
            )
        };
        // SAFETY: no pointers; a successful begin is ended once, in `Drop`.
        let began = unsafe { timeBeginPeriod(1) } == TIMERR_NOERROR;
        Self { began }
    }
    /// Whether Windows accepted the one-millisecond period.
    pub const fn active(&self) -> bool {
        self.began
    }
}
impl Drop for FineTimer {
    fn drop(&mut self) {
        if self.began {
            // SAFETY: ends the period this value began.
            unsafe { timeEndPeriod(1) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn advances_with_real_time() {
        let (first, started) = (since_boot_ns(), Instant::now());
        std::thread::sleep(Duration::from_millis(50));
        let (second, elapsed) = (since_boot_ns(), started.elapsed());
        let advanced = Duration::from_nanos(second - first);
        assert!(advanced >= Duration::from_millis(45), "{advanced:?}");
        assert!(
            advanced <= elapsed + Duration::from_millis(5),
            "{advanced:?}"
        );
    }

    /// The median of 40 timed waits of 2 ms each. Win32 `Sleep` waits on the
    /// system tick as the reactor's completion-port waits do (Rust's
    /// `thread::sleep` uses a high-resolution timer instead).
    fn median_wait() -> Duration {
        let mut waits: Vec<Duration> = (0..40)
            .map(|_| {
                let started = Instant::now();
                // SAFETY: no pointers.
                unsafe { windows::Win32::System::Threading::Sleep(2) };
                started.elapsed()
            })
            .collect();
        waits.sort();
        waits[waits.len() / 2]
    }

    /// A 2 ms wait takes about 2 ms while the timer lives. Without it this
    /// test process waits on the default 15.625 ms tick (the planted negative,
    /// measured first; another process's period does not shorten this one's
    /// waits on Windows 10 2004 and later).
    #[test]
    fn short_waits_keep_time_while_the_timer_lives() {
        let coarse = median_wait();
        let timer = FineTimer::begin();
        assert!(timer.active());
        let fine = median_wait();
        drop(timer);
        assert!(fine < Duration::from_millis(5), "fine {fine:?}");
        assert!(coarse > Duration::from_millis(10), "coarse {coarse:?}");
    }
}
