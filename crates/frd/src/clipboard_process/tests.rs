//! The real frd side (spawn, socketpair, bounded payloads, deadline
//! translation, timeouts, poisoning, kill/reap custody) against a Python
//! PROTOCOL fixture child. The fixture owns no X11 selection; real X11 effects
//! are qualified by fr-native's Xvfb test and the namespace e2e.
use super::*;
use crate::input_watchdog::host_now;
use asupersync::{
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
};
use fr_core::{clipboard::Endpoint, time::HostDuration};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Write the fixture with its mode and a fresh log path. Returns both paths.
pub(crate) fn fixture(mode: &str) -> (PathBuf, PathBuf) {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let base = std::env::temp_dir().join(format!(
        "fr-clipboard-fixture-{}-{n}-{mode}",
        std::process::id()
    ));
    let image = base.with_extension("py");
    let source = base.with_extension("src");
    let log = base.with_extension("log");
    let script = include_str!("clipboard_fixture.py")
        .replace("@MODE@", mode)
        .replace("@LOG@", log.to_str().unwrap());
    std::fs::write(&source, script).unwrap();
    // A separate process writes the executable: this multi-threaded test
    // process never holds a writable descriptor to it, so a concurrent fork
    // in another test cannot make its exec fail with ETXTBSY.
    assert!(
        std::process::Command::new("cp")
            .arg(&source)
            .arg(&image)
            .status()
            .unwrap()
            .success()
    );
    std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o700)).unwrap();
    (image, log)
}
pub(crate) fn transcript(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}
/// A local application's copy on the fixture's stand-in clipboard.
pub(crate) fn local_copy(log: &Path, text: &str) {
    let path = PathBuf::from(format!("{}.copy", log.display()));
    std::fs::write(path, text).unwrap();
}
fn runtime() -> Runtime {
    RuntimeBuilder::new().worker_threads(1).build().unwrap()
}
fn launch(image: &Path) -> ProcessLaunch {
    ProcessLaunch::new(image, ":0", None, 0x5eed_cafe).unwrap()
}
fn stamp(sequence: u64) -> Stamp {
    Stamp {
        id: 0x1234_5678_9abc_def0,
        source: Endpoint::Controller,
        sequence,
    }
}
fn later(cx: &Cx, micros: u64) -> HostInstant {
    host_now(cx)
        .unwrap()
        .checked_add(HostDuration::from_micros(micros))
        .unwrap()
}
fn line(log: &Path, prefix: &str) -> Option<String> {
    transcript(log).into_iter().find(|l| l.starts_with(prefix))
}

#[test]
fn a_local_copy_is_read_and_a_published_item_keeps_its_stamp() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let (image, log) = fixture("normal");
    let mut owner = RemoteClipboard::start(&launch(&image), cx.clone(), 4096).unwrap();
    // The child runs with a cleared environment: DISPLAY only (no XAUTHORITY
    // was configured), and the launch arguments name the clipboard role.
    // DISPLAY only (Python's own C-locale coercion may add LC_CTYPE).
    let start = line(&log, "START").unwrap();
    let env = start.rsplit(' ').next().unwrap();
    assert!(
        env.split(',').all(|k| k == "DISPLAY" || k == "LC_CTYPE"),
        "{env}"
    );
    assert!(env.split(',').any(|k| k == "DISPLAY"));
    assert_eq!(line(&log, "HELLO").as_deref(), Some("HELLO 4096"));
    let bootstrap = owner.watch().unwrap();
    assert_eq!(bootstrap, 1);
    let first = owner.changes().unwrap();
    assert!(first.settled);
    let first = first.latest.unwrap();
    assert_eq!((first.revision, first.has_selection), (1, false));
    // A local application copies: the next turn reports it, then one read.
    let copied = "héllo λ 👋 from the host";
    local_copy(&log, copied);
    let change = owner.changes().unwrap().latest.unwrap();
    assert!(change.has_selection && change.origin.is_none());
    assert_eq!(owner.revision(), change.revision);
    owner.begin_read().unwrap();
    let text = owner.poll_read().unwrap().unwrap();
    assert_eq!(text.text(), copied);
    assert_eq!(text.origin(), None);
    // A peer item: prepared against the current revision, then published
    // with the core's deadline translated into the child's monotonic clock.
    let item = "from the viewer ✓";
    owner
        .prepare_for_revision(item, stamp(9), owner.revision())
        .unwrap();
    assert_eq!(
        owner.publish_until(item, stamp(9), later(&cx, 500_000)),
        Publication::SubmittedToOs
    );
    assert_eq!(
        line(&log, "PUBLISH ").as_deref(),
        Some(format!("PUBLISH {}", item.len()).as_str())
    );
    let window: i64 = line(&log, "PUBLISH-WINDOW")
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    // Derived from `until` (errs early), never a fixed or widened window.
    assert!(window > 0 && window <= 500_000_000, "{window}");
    // Our own publication is reported with its exact provenance stamp.
    let own = owner.changes().unwrap().latest.unwrap();
    assert_eq!(own.origin, Some(stamp(9)));
    owner.begin_read().unwrap();
    let echo = owner.poll_read().unwrap().unwrap();
    assert_eq!((echo.text(), echo.origin()), (item, Some(stamp(9))));
    // A stale revision is refused by the child's revision fence.
    assert_eq!(
        owner.prepare_for_revision(item, stamp(10), own.revision - 1),
        Err(PlatformError::LocalChanged)
    );
    NativeClipboard::close(&mut owner);
    assert_eq!(transcript(&log).last().map(String::as_str), Some("STOP"));
    assert!(owner.child.is_none(), "reaped");
    // Idempotent and typed after close; no I/O is attempted.
    NativeClipboard::close(&mut owner);
    assert_eq!(owner.watch(), Err(RemoteError::Channel));
}

#[test]
fn a_publication_without_a_live_deadline_never_reaches_the_x11_owner() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let (image, log) = fixture("normal");
    let mut owner = RemoteClipboard::start(&launch(&image), cx.clone(), 4096).unwrap();
    owner.watch().unwrap();
    owner.prepare("peer item", stamp(1)).unwrap();
    // No deadline: fail closed, nothing sent.
    assert_eq!(
        owner.publish("peer item", stamp(1)),
        Publication::NotSubmitted(PlatformError::Unavailable)
    );
    // An already-passed deadline (a revoked or expired lease): nothing sent.
    let past = host_now(&cx).unwrap();
    assert_eq!(
        owner.publish_until("peer item", stamp(1), past),
        Publication::NotSubmitted(PlatformError::Unavailable)
    );
    assert!(line(&log, "PUBLISH").is_none(), "{:?}", transcript(&log));
    assert!(!owner.is_poisoned(), "a refusal is not a channel failure");
    // The core's guard cancels the retained preparation.
    owner.cancel_prepared();
    drop(owner);
    assert_eq!(transcript(&log).last().map(String::as_str), Some("STOP"));

    // A deadline that passes while the request waits in the child: the child's
    // own final check (in its CLOCK_MONOTONIC) refuses the ownership call.
    let (image, log) = fixture("late");
    let mut owner = RemoteClipboard::start(&launch(&image), cx.clone(), 4096).unwrap();
    owner.watch().unwrap();
    owner.prepare("peer item", stamp(2)).unwrap();
    assert_eq!(
        owner.publish_until("peer item", stamp(2), later(&cx, 50_000)),
        Publication::NotSubmitted(PlatformError::Unavailable)
    );
    assert!(
        line(&log, "PUBLISH-LATE").is_some(),
        "{:?}",
        transcript(&log)
    );
    assert!(line(&log, "PUBLISH ").is_none());
}

#[test]
fn an_oversized_item_is_refused_before_allocation_in_either_direction() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let (image, log) = fixture("oversize");
    let mut owner = RemoteClipboard::start(&launch(&image), cx.clone(), 16).unwrap();
    owner.watch().unwrap();
    // Outgoing: refused before any exchange (the child never sees it).
    assert_eq!(
        owner.prepare(&"x".repeat(17), stamp(1)),
        Err(PlatformError::Unsupported)
    );
    assert!(line(&log, "PREPARE").is_none());
    assert!(!owner.is_poisoned());
    owner.prepare(&"x".repeat(16), stamp(1)).unwrap();
    assert_eq!(line(&log, "PREPARE").as_deref(), Some("PREPARE 16"));
    // Incoming: the child announces 17 bytes; the header alone is refused,
    // before any reservation or payload read, and the channel is poisoned.
    local_copy(&log, "small");
    owner.changes().unwrap();
    owner.begin_read().unwrap();
    assert!(matches!(owner.poll_read(), Err(RemoteError::Channel)));
    assert_eq!(line(&log, "READ").as_deref(), Some("READ 17"));
    assert!(owner.is_poisoned());
    // Terminal: typed failures, no further requests, no resend.
    assert_eq!(owner.changes().err(), Some(RemoteError::Channel));
    let before = transcript(&log).len();
    NativeClipboard::close(&mut owner);
    assert_eq!(
        transcript(&log).len(),
        before,
        "no Stop to a poisoned child"
    );
    assert!(owner.child.is_none(), "killed and reaped");
    // The item bound can never exceed the protocol ceiling.
    assert_eq!(
        RemoteClipboard::start(&launch(&image), cx, process::MAX_ITEM_BYTES + 1).err(),
        Some(PlatformError::Unsupported)
    );
}

#[test]
fn a_hung_malformed_or_refusing_child_fails_typed_and_is_killed() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let (image, log) = fixture("hang");
    let mut owner = RemoteClipboard::start(&launch(&image), cx.clone(), 4096).unwrap();
    owner.watch().unwrap();
    local_copy(&log, "text");
    owner.changes().unwrap();
    owner.begin_read().unwrap();
    let started = Instant::now();
    assert!(matches!(owner.poll_read(), Err(RemoteError::Channel)));
    let waited = started.elapsed();
    assert!(
        waited >= REPLY_TIMEOUT && waited < REPLY_TIMEOUT + Duration::from_secs(2),
        "{waited:?}"
    );
    assert!(owner.is_poisoned());
    drop(owner);
    assert!(line(&log, "HANG").is_some());

    let (image, log) = fixture("badutf8");
    let mut owner = RemoteClipboard::start(&launch(&image), cx.clone(), 4096).unwrap();
    owner.watch().unwrap();
    local_copy(&log, "text");
    owner.changes().unwrap();
    owner.begin_read().unwrap();
    assert!(matches!(owner.poll_read(), Err(RemoteError::Channel)));
    assert!(owner.is_poisoned());

    let (image, log) = fixture("refuse");
    assert_eq!(
        RemoteClipboard::start(&launch(&image), cx.clone(), 4096).err(),
        Some(PlatformError::Unsupported)
    );
    assert_eq!(line(&log, "HELLO").as_deref(), Some("HELLO 4096"));
    // A missing image is a typed refusal too; nothing to reap.
    assert_eq!(
        RemoteClipboard::start(&launch(Path::new("/nonexistent/fr-input-agent")), cx, 4096).err(),
        Some(PlatformError::Unsupported)
    );
}

#[test]
fn debug_and_errors_never_contain_clipboard_text() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let (image, log) = fixture("normal");
    let mut owner = RemoteClipboard::start(&launch(&image), cx, 4096).unwrap();
    owner.watch().unwrap();
    let secret = "correct horse battery staple";
    local_copy(&log, secret);
    owner.changes().unwrap();
    owner.begin_read().unwrap();
    let text = owner.poll_read().unwrap().unwrap();
    assert_eq!(text.text(), secret);
    let rendered = [
        format!("{owner:?}"),
        format!("{text:?}"),
        format!("{:?}", RemoteError::Native(Failure::InvalidUtf8)),
        format!("{:?}", RemoteError::Channel),
        format!("{:?}", owner.poll_read().err()),
    ];
    for rendered in rendered {
        assert!(!rendered.contains("horse"), "{rendered}");
    }
    // Neither the fixture nor this side ever logs the text.
    let log = std::fs::read_to_string(&log).unwrap();
    assert!(!log.contains("horse"), "{log}");
}
