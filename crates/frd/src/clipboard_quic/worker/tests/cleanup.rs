//! Actual worker-thread retention, with native completion controlled separately
//! from cancellation. No Windows UI or native clipboard qualification is claimed.
use super::*;
use std::time::{Duration, Instant};

struct Scenario {
    seed: Option<WorkerSeed>,
    os: Arc<Mutex<Os>>,
    _runtime: Runtime,
}

impl Scenario {
    fn new() -> Self {
        let clock = Arc::new(VirtualClock::new());
        let runtime = RuntimeBuilder::new()
            .worker_threads(1)
            .with_timer_driver(TimerDriverHandle::with_virtual_clock(clock))
            .build()
            .unwrap();
        let channel = ChannelSession::with_monitor(
            Monitor::new(Owner(Arc::default())),
            context(),
            ProtocolLimits::ABSOLUTE,
            true,
            HostInstant::ORIGIN,
        )
        .unwrap();
        let gate = channel.transport();
        let shared = Arc::new(Shared {
            gate: gate.clone(),
            maximum: ProtocolLimits::ABSOLUTE.max_control_message_bytes() as usize,
            inbox: Mutex::default(),
            outbox: Mutex::default(),
            inbound_until: std::sync::atomic::AtomicU64::default(),
            inbound_bound: std::sync::atomic::AtomicU64::default(),
            wake: std::sync::OnceLock::default(),
        });
        Self {
            seed: Some(WorkerSeed {
                session: Some(Session::Host(channel)),
                shared,
                clock: Clock::Host(gate),
                cx: runtime.request_cx_with_budget(Budget::INFINITE),
            }),
            os: Arc::new(Mutex::new(Os {
                cleanup_pending: true,
                ..Os::default()
            })),
            _runtime: runtime,
        }
    }

    fn spawn(&mut self, stop_during_open: bool) -> WorkerTask {
        let seed = self.seed.take().unwrap();
        let stop = seed.control();
        let os = self.os.clone();
        seed.spawn(
            move || {
                os.lock().unwrap().opens += 1;
                if stop_during_open {
                    stop.stop();
                }
                Ok(Recording {
                    os,
                    seen: 0,
                    reading: false,
                })
            },
            || Ok(91),
        )
        .unwrap()
    }

    fn release(&self) {
        self.os.lock().unwrap().cleanup_pending = false;
    }

    fn wait_for_close(&self) {
        wait(|| self.os.lock().unwrap().cleanup_polls != 0);
        assert_eq!(self.os.lock().unwrap().closes, 1);
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        // A failed test must not strand its explicitly controlled native wait.
        self.release();
    }
}

fn wait(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(2);
    while !condition() {
        assert!(
            Instant::now() < until,
            "original clipboard worker did not progress"
        );
        std::thread::park_timeout(Duration::from_millis(1));
    }
}

#[test]
fn stop_retains_original_worker_until_native_cleanup_finishes() {
    let mut scenario = Scenario::new();
    let mut task = scenario.spawn(false);
    wait(|| task.is_ready());
    task.stop();
    scenario.wait_for_close();
    for _ in 0..3 {
        assert_eq!(task.finish(), None);
        assert!(!task.is_finished());
        task.stop();
    }
    scenario.release();
    wait(|| task.is_finished());
    assert_eq!(task.finish(), Some(Ok(())));
    assert_eq!(task.finish(), None);
    let os = scenario.os.lock().unwrap();
    assert_eq!((os.opens, os.closes), (1, 1));
}

#[test]
fn native_cleanup_failure_is_reported_after_the_original_worker_finishes() {
    let mut scenario = Scenario::new();
    scenario.os.lock().unwrap().cleanup_failed = true;
    let mut task = scenario.spawn(false);
    wait(|| task.is_ready());
    task.stop();
    scenario.wait_for_close();
    assert_eq!(task.finish(), None);
    scenario.release();
    wait(|| task.is_finished());
    assert_eq!(task.finish(), Some(Err(Error::NativeCleanup)));
}

#[test]
fn native_step_failure_still_retains_pending_cleanup() {
    let mut scenario = Scenario::new();
    scenario.os.lock().unwrap().watch_failed = true;
    let mut task = scenario.spawn(false);
    scenario.wait_for_close();
    assert_eq!(task.finish(), None);
    scenario.release();
    wait(|| task.is_finished());
    assert_eq!(task.finish(), Some(Err(Error::Native)));
    assert_eq!(scenario.os.lock().unwrap().closes, 1);
}

#[test]
fn cancelled_factory_result_is_not_ready_or_reaped_before_native_cleanup() {
    let mut scenario = Scenario::new();
    let mut task = scenario.spawn(true);
    scenario.wait_for_close();
    assert!(!task.is_ready());
    assert_eq!(task.finish(), None);
    scenario.release();
    wait(|| task.is_finished());
    assert!(matches!(task.finish(), Some(Err(Error::Session(_)))));
    let os = scenario.os.lock().unwrap();
    assert_eq!((os.opens, os.watches, os.closes), (1, 0, 1));
}

#[test]
fn cleanup_failure_survives_an_earlier_native_step_failure() {
    let mut scenario = Scenario::new();
    {
        let mut os = scenario.os.lock().unwrap();
        os.watch_failed = true;
        os.cleanup_failed = true;
    }
    let mut task = scenario.spawn(false);
    scenario.wait_for_close();
    scenario.release();
    wait(|| task.is_finished());
    assert_eq!(task.finish(), Some(Err(Error::NativeCleanup)));
}

#[test]
fn failed_factory_keeps_cleanup_distinct_from_platform_unavailability() {
    for (native, expected) in [
        (
            NativeOpenError::Native(PlatformError::Unavailable),
            Error::NativeSetup(PlatformError::Unavailable),
        ),
        (NativeOpenError::Cleanup, Error::NativeCleanup),
    ] {
        let mut scenario = Scenario::new();
        let (release, wait_for_native) = std::sync::mpsc::channel();
        let entered = Arc::new(AtomicBool::new(false));
        let opening = entered.clone();
        let mut task = scenario
            .seed
            .take()
            .unwrap()
            .spawn_with_cleanup::<Recording, _, _>(
                move || {
                    opening.store(true, Ordering::Release);
                    let _ = wait_for_native.recv();
                    Err(native)
                },
                || Ok(91),
            )
            .unwrap();
        wait(|| entered.load(Ordering::Acquire));
        task.stop();
        let pending = task.finish();
        let ready = task.is_ready();
        release.send(()).unwrap();
        assert_eq!(pending, None);
        assert!(!ready);
        wait(|| task.is_finished());
        assert_eq!(task.finish(), Some(Err(expected)));
    }
}
