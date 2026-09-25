//! Raw `CLOCK_MONOTONIC` for the out-of-process input executor's final check.
//! The broker translates its authority deadline into this clock domain (the
//! Asupersync wall-clock timer ticks at the same rate); it is never wall time.
use core::ffi::{c_int, c_long};

#[repr(C)]
struct Timespec {
    tv_sec: c_long,
    tv_nsec: c_long,
}
unsafe extern "C" {
    fn clock_gettime(clock: c_int, now: *mut Timespec) -> c_int;
}
/// Linux UAPI `CLOCK_MONOTONIC`.
const CLOCK_MONOTONIC: c_int = 1;

/// Nanoseconds since an arbitrary per-boot origin. `None` never happens on a
/// supported kernel; callers treat it as "deadline unknown", i.e. expired.
pub fn monotonic_ns() -> Option<u64> {
    let mut now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid clock id and a writable, correctly laid out timespec that
    // lives across the call; the kernel/vDSO retains no pointer.
    if unsafe { clock_gettime(CLOCK_MONOTONIC, &raw mut now) } != 0 {
        return None;
    }
    u64::try_from(now.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(now.tv_nsec).ok()?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn monotonic_clock_never_regresses_and_tracks_std_instant() {
        let start = std::time::Instant::now();
        let a = super::monotonic_ns().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = super::monotonic_ns().unwrap();
        let elapsed = u64::try_from(start.elapsed().as_nanos()).unwrap();
        assert!(b >= a + 20_000_000);
        // Same clock as std's Instant (and the runtime's wall-clock timer).
        assert!(b - a <= elapsed);
    }
}
