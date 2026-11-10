//! A host started before its macOS grants waits for them in-process
//! ([`awaiting_permission`]) instead of exiting into launchd's restart loop.
use super::*;
use std::sync::Mutex;

fn screen_recording() -> Error {
    Error::EncoderUnavailable("videotoolbox=screen_recording_not_granted".to_owned())
}

/// Records every reported event; the wait's own pause is 1 ms.
fn harness() -> (Reporter, Arc<Mutex<Vec<Event>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let report: Reporter = Arc::new(move |event| sink.lock().unwrap().push(event));
    (report, events)
}
fn fast(_: u32) -> Duration {
    Duration::from_millis(1)
}

#[test]
fn the_capture_and_control_refusals_name_their_permission_and_nothing_else_does() {
    assert_eq!(
        screen_recording().missing_permission(),
        Some(Permission::ScreenRecording)
    );
    assert_eq!(
        Error::InputAgentUnavailable(PlatformError::Permission).missing_permission(),
        Some(Permission::Accessibility)
    );
    for other in [
        Error::EncoderUnavailable("videotoolbox_timed_out".to_owned()),
        Error::EncoderUnavailable("videotoolbox=session_failed".to_owned()),
        // Two backends, one missing a grant: not a pure permission wait.
        Error::EncoderUnavailable(
            "videotoolbox=screen_recording_not_granted;nvenc=unavailable".to_owned(),
        ),
        Error::InputAgentUnavailable(PlatformError::Unavailable),
        Error::InputAgentUnavailable(PlatformError::Unsupported),
        Error::Configuration,
    ] {
        assert_eq!(other.missing_permission(), None, "{other:?}");
    }
    assert_eq!(Permission::ScreenRecording.as_str(), "screen_recording");
    assert_eq!(Permission::Accessibility.settings_name(), "Accessibility");
}

#[test]
fn only_macos_waits_for_a_permission() {
    let waits = awaited_permission(&screen_recording());
    if cfg!(target_os = "macos") {
        assert_eq!(waits, Some(Permission::ScreenRecording));
    } else {
        assert_eq!(waits, None);
    }
}

#[test]
fn a_missing_grant_is_reported_once_and_rechecked_until_it_is_allowed() {
    let (report, events) = harness();
    let stop = StopHandle::default();
    let mut calls = 0;
    let result = awaiting_permission(&stop, &report, Error::missing_permission, fast, || {
        calls += 1;
        if calls < 4 {
            Err(screen_recording())
        } else {
            Ok(calls)
        }
    });
    assert_eq!(result, Ok(Some(4)));
    assert_eq!(
        *events.lock().unwrap(),
        [Event::AwaitingPermission {
            permission: Permission::ScreenRecording
        }]
    );
}

#[test]
fn any_other_refusal_returns_at_once_without_waiting() {
    let (report, events) = harness();
    let stop = StopHandle::default();
    let mut calls = 0;
    let result: Result<Option<()>, Error> =
        awaiting_permission(&stop, &report, Error::missing_permission, fast, || {
            calls += 1;
            Err(Error::EncoderUnavailable(
                "videotoolbox_timed_out".to_owned(),
            ))
        });
    assert_eq!(
        result,
        Err(Error::EncoderUnavailable(
            "videotoolbox_timed_out".to_owned()
        ))
    );
    assert_eq!(calls, 1);
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn a_platform_that_never_waits_returns_the_permission_refusal_itself() {
    let (report, events) = harness();
    let stop = StopHandle::default();
    let result: Result<Option<()>, Error> =
        awaiting_permission(&stop, &report, |_| None, fast, || Err(screen_recording()));
    assert_eq!(result, Err(screen_recording()));
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn a_stop_while_waiting_ends_the_wait_without_another_probe() {
    let (report, events) = harness();
    let stop = Arc::new(StopHandle::default());
    let mut calls = 0;
    let stopper = Arc::clone(&stop);
    let result: Result<Option<()>, Error> = awaiting_permission(
        &stop,
        &report,
        Error::missing_permission,
        // A long pause: only the stop can end it.
        |_| Duration::from_secs(600),
        || {
            calls += 1;
            stopper.request();
            Err(Error::InputAgentUnavailable(PlatformError::Permission))
        },
    );
    assert_eq!(result, Ok(None));
    assert_eq!(calls, 1);
    assert_eq!(
        *events.lock().unwrap(),
        [Event::AwaitingPermission {
            permission: Permission::Accessibility
        }]
    );
}

#[test]
fn granting_one_permission_then_missing_the_next_reports_each_once() {
    let (report, events) = harness();
    let stop = StopHandle::default();
    let mut calls = 0;
    let result = awaiting_permission(&stop, &report, Error::missing_permission, fast, || {
        calls += 1;
        match calls {
            1 | 2 => Err(Error::InputAgentUnavailable(PlatformError::Permission)),
            3 | 4 => Err(screen_recording()),
            _ => Ok(()),
        }
    });
    assert_eq!(result, Ok(Some(())));
    assert_eq!(
        *events.lock().unwrap(),
        [
            Event::AwaitingPermission {
                permission: Permission::Accessibility
            },
            Event::AwaitingPermission {
                permission: Permission::ScreenRecording
            },
        ]
    );
}

#[test]
fn rechecks_are_quick_for_the_first_minute_then_relaxed() {
    assert_eq!(permission_recheck(1), Duration::from_secs(5));
    assert_eq!(permission_recheck(12), Duration::from_secs(5));
    assert_eq!(permission_recheck(13), Duration::from_secs(30));
    assert_eq!(permission_recheck(u32::MAX), Duration::from_secs(30));
}
