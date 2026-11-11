//! Explicit kernel clock domains for private native process boundaries.
//! Input uses suspend-inclusive time; existing audio, microphone and clipboard
//! protocols retain their separate raw monotonic clock. Neither is wall time.
use core::ffi::{c_int, c_long};

#[repr(C)]
struct Timespec {
    tv_sec: c_long,
    tv_nsec: c_long,
}
unsafe extern "C" {
    fn clock_gettime(clock: c_int, now: *mut Timespec) -> c_int;
}
/// `CLOCK_MONOTONIC` (Linux UAPI 1; Darwin `<time.h>` 6). Darwin's includes
/// sleep, unlike std's `Instant` (`CLOCK_UPTIME_RAW`); Linux's excludes it.
#[cfg(target_os = "linux")]
const CLOCK_MONOTONIC: c_int = 1;
#[cfg(target_os = "macos")]
const CLOCK_MONOTONIC: c_int = 6;

/// Linux `CLOCK_BOOTTIME` includes suspend. On Darwin the existing monotonic
/// domain already does so. These are the version-2 input IPC clock domains.
#[cfg(target_os = "linux")]
const INPUT_AUTHORITY_CLOCK: c_int = 7;
#[cfg(target_os = "macos")]
const INPUT_AUTHORITY_CLOCK: c_int = CLOCK_MONOTONIC;

/// Final input submission time, including any sleep while an original command
/// was queued or its native preflight was blocked. Never replace this with
/// Linux `CLOCK_MONOTONIC` or `Instant`: neither includes suspended time.
pub fn input_authority_ns() -> Option<u64> {
    clock_ns(INPUT_AUTHORITY_CLOCK)
}

/// Nanoseconds since an arbitrary per-boot origin. `None` never happens on a
/// supported kernel; callers treat it as "deadline unknown", i.e. expired.
pub fn monotonic_ns() -> Option<u64> {
    clock_ns(CLOCK_MONOTONIC)
}

fn clock_ns(clock: c_int) -> Option<u64> {
    let mut now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid clock id and a writable, correctly laid out timespec that
    // lives across the call; the kernel/vDSO retains no pointer.
    if unsafe { clock_gettime(clock, &raw mut now) } != 0 {
        return None;
    }
    let nanos = u64::try_from(now.tv_nsec)
        .ok()
        .filter(|nanos| *nanos < 1_000_000_000)?;
    u64::try_from(now.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(nanos)
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
    #[test]
    fn input_clock_reads_the_platform_suspend_inclusive_kernel_domain() {
        // This executes the actual platform clock, not a simulated suspend.
        // Correlation/suspend semantics are tested separately in fr-core.
        #[cfg(target_os = "linux")]
        let expected = 7; // Linux UAPI CLOCK_BOOTTIME, required by input IPC v2.
        #[cfg(target_os = "macos")]
        let expected = 6; // Darwin CLOCK_MONOTONIC includes sleep.
        assert_eq!(super::INPUT_AUTHORITY_CLOCK, expected);
        let before = super::clock_ns(expected).unwrap();
        let input = super::input_authority_ns().unwrap();
        let after = super::clock_ns(expected).unwrap();
        assert!(before <= input && input <= after);
    }

    #[test]
    fn input_clock_cannot_lag_the_existing_monotonic_protocol_clock() {
        let monotonic = super::monotonic_ns().unwrap();
        let input = super::input_authority_ns().unwrap();
        // Linux's BOOTTIME adds suspended time to MONOTONIC. Darwin uses the
        // same suspend-inclusive clock for both private profiles.
        assert!(input >= monotonic);
    }

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
    fn monotonic_clock_never_regresses_and_includes_an_awake_delay() {
        #[cfg(target_os = "linux")]
        let start = std::time::Instant::now();
        let a = super::monotonic_ns().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = super::monotonic_ns().unwrap();
        assert!(b >= a + 20_000_000);
        #[cfg(target_os = "linux")]
        {
            let elapsed = u64::try_from(start.elapsed().as_nanos()).unwrap();
            // Linux Instant shares this clock; macOS Instant excludes sleep
            // while the unchanged broker/child CLOCK_MONOTONIC includes it.
            assert!(b - a <= elapsed);
        }
    }
}
