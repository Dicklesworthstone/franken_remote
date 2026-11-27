//! Explicit kernel clock domains for private native process boundaries.
//! Input, client microphone capture and local playback use suspend-inclusive time.
//! Host capture, virtual-mic playout and clipboard have separate private clocks.
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
    suspend_inclusive_ns()
}

/// Same-boot local time for deadlines that must continue aging during sleep.
pub fn suspend_inclusive_ns() -> Option<u64> {
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

/// Fixed local and suspend-inclusive bounds for one microphone operation.
/// Narrowing an existing batch (including a partial PCM frame) never restamps
/// either bound. Exactly reaching either deadline is terminal.
#[cfg(any(feature = "microphone-process", feature = "linux-microphone-capture"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CaptureDeadline {
    pub(crate) until: std::time::Instant,
    pub(crate) ns: u64,
}

#[cfg(any(feature = "microphone-process", feature = "linux-microphone-capture"))]
impl CaptureDeadline {
    pub(crate) fn narrow(self, other: Self) -> Self {
        Self {
            until: self.until.min(other.until),
            ns: self.ns.min(other.ns),
        }
    }

    pub(crate) fn duration_at(
        self,
        now: std::time::Instant,
        raw: u64,
    ) -> Option<std::time::Duration> {
        let left = self
            .until
            .checked_duration_since(now)?
            .min(std::time::Duration::from_nanos(self.ns.checked_sub(raw)?));
        (!left.is_zero()).then_some(left)
    }
}

/// Translate an original local instant to the same-boot child clock. Sampling
/// raw time FIRST conservatively places the result no later than `at`; time
/// spent crossing this boundary never adds custody or authorization lifetime.
#[cfg(any(feature = "microphone-process", feature = "linux-microphone-capture"))]
pub(crate) fn stamp_ns(at: std::time::Instant) -> Option<u64> {
    let raw = suspend_inclusive_ns()?;
    let local = std::time::Instant::now();
    if let Some(ahead) = at.checked_duration_since(local) {
        raw.checked_add(u64::try_from(ahead.as_nanos()).ok()?)
    } else {
        raw.checked_sub(u64::try_from(local.duration_since(at).as_nanos()).ok()?)
    }
}

/// Translate an original child's absolute instant back without renewing it.
/// Local time is sampled FIRST, so this conversion can only shorten lifetime.
#[cfg(feature = "microphone-process")]
pub(crate) fn capture_local_instant(at_ns: u64) -> Option<std::time::Instant> {
    local_from_clock(at_ns, suspend_inclusive_ns)
}

/// The separate virtual microphone playout IPC retains its original clock.
#[cfg(feature = "linux-microphone-playout")]
pub(crate) fn local_instant(at_ns: u64) -> Option<std::time::Instant> {
    local_from_clock(at_ns, monotonic_ns)
}

#[cfg(any(feature = "microphone-process", feature = "linux-microphone-playout"))]
fn local_from_clock(at_ns: u64, read: fn() -> Option<u64>) -> Option<std::time::Instant> {
    let local = std::time::Instant::now();
    let raw = read()?;
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

    #[cfg(feature = "microphone-process")]
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
            let returned = super::capture_local_instant(raw).unwrap();
            assert!(returned <= at);
            assert!(at.duration_since(returned) < std::time::Duration::from_millis(20));
        }
    }

    #[cfg(feature = "microphone-process")]
    #[test]
    fn original_microphone_request_and_partial_batch_expire_on_the_first_sleeping_clock() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        let first = super::CaptureDeadline {
            until: now + Duration::from_millis(90),
            ns: 1_090_000_000,
        };
        let second = super::CaptureDeadline {
            until: now + Duration::from_millis(30),
            ns: 1_095_000_000,
        };
        let retained = first.narrow(second);
        assert_eq!(
            retained.duration_at(now, 1_089_999_999),
            Some(Duration::from_nanos(1))
        );
        assert_eq!(retained.duration_at(now, 1_090_000_000), None);
        assert_eq!(
            retained.duration_at(now + Duration::from_millis(30), 1_000_000_000),
            None
        );
    }

    #[cfg(all(feature = "microphone-process", target_os = "linux"))]
    #[test]
    fn microphone_private_stamp_matches_boottime_even_after_an_earlier_suspend() {
        use std::time::{Duration, Instant};
        let before = super::clock_ns(7).unwrap();
        let local = Instant::now();
        let expiry = local + Duration::from_secs(1);
        let stamp = super::stamp_ns(expiry).unwrap();
        let resumed = super::capture_local_instant(stamp).unwrap();
        let after = super::clock_ns(7).unwrap();
        let uncertainty = u64::try_from(local.elapsed().as_nanos()).unwrap();
        assert!(stamp >= before + 1_000_000_000 - uncertainty.min(1_000_000_000));
        assert!(stamp <= after + 1_000_000_000);
        assert!(resumed <= expiry);
        assert!(expiry - resumed <= local.elapsed());
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
