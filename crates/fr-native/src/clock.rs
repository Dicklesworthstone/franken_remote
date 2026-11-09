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
/// `CLOCK_MONOTONIC`: the clock frd's deadline translation reads (Linux UAPI
/// 1; Darwin `<time.h>` 6, which counts across sleep on both sides alike).
#[cfg(target_os = "linux")]
const CLOCK_MONOTONIC: c_int = 1;
#[cfg(target_os = "macos")]
const CLOCK_MONOTONIC: c_int = 6;

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

/// Translate an original local instant to the same-boot child clock. Sampling
/// raw time FIRST conservatively places the result no later than `at`; time
/// spent crossing this boundary never adds custody or authorization lifetime.
#[cfg(feature = "linux-microphone-process")]
pub(crate) fn stamp_ns(at: std::time::Instant) -> Option<u64> {
    let raw = monotonic_ns()?;
    let local = std::time::Instant::now();
    if let Some(ahead) = at.checked_duration_since(local) {
        raw.checked_add(u64::try_from(ahead.as_nanos()).ok()?)
    } else {
        raw.checked_sub(u64::try_from(local.duration_since(at).as_nanos()).ok()?)
    }
}

/// Translate an original child's absolute instant back without renewing it.
/// Local time is sampled FIRST, so this conversion can only shorten lifetime.
#[cfg(any(
    feature = "linux-microphone-process",
    feature = "linux-microphone-playout"
))]
pub(crate) fn local_instant(at_ns: u64) -> Option<std::time::Instant> {
    let local = std::time::Instant::now();
    let raw = monotonic_ns()?;
    if let Some(ahead) = at_ns.checked_sub(raw) {
        local.checked_add(std::time::Duration::from_nanos(ahead))
    } else {
        local.checked_sub(std::time::Duration::from_nanos(raw - at_ns))
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "linux-microphone-process")]
    #[test]
    fn process_handoffs_only_shorten_original_instants() {
        let now = std::time::Instant::now();
        for at in [
            now.checked_sub(std::time::Duration::from_millis(80))
                .unwrap(),
            now,
            now + std::time::Duration::from_millis(80),
        ] {
            let raw = super::stamp_ns(at).unwrap();
            let returned = super::local_instant(raw).unwrap();
            assert!(returned <= at);
            assert!(at.duration_since(returned) < std::time::Duration::from_millis(20));
        }
    }

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
