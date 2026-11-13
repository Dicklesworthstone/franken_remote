//! A clock that keeps counting across sleep and hibernation and that no one
//! can set: the Windows counterpart of Linux's `CLOCK_BOOTTIME`, for evidence
//! that must expire while the machine sleeps.
use windows::Win32::System::WindowsProgramming::QueryInterruptTimePrecise;

/// Nanoseconds since boot, sleep included (interrupt time, which is biased
/// by suspended time; the "unbiased" variant leaves it out).
pub fn since_boot_ns() -> u64 {
    // SAFETY: a counter read with no arguments; it cannot fail.
    let ticks = unsafe { QueryInterruptTimePrecise() };
    // 100 ns units; u64 nanoseconds last 584 years of uptime.
    ticks.saturating_mul(100)
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
}
