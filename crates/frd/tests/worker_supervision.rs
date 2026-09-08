#![cfg(target_os = "linux")]
use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    types::{Budget, CancelKind},
};
use fr_core::ids::CodecConfigurationGeneration;
use fr_media::worker::{Backend, Configuration, Kind, Role};
use frd::worker::{Deadline, Error, Launch, State, Worker};
use std::{
    future::{Future, poll_fn},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    pin::pin,
    task::Poll,
    time::Duration,
};
fn runtime() -> Runtime {
    RuntimeBuilder::new()
        .worker_threads(1)
        .blocking_threads(1, 2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
fn config() -> Configuration {
    Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: 1024 * 1024,
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
fn fixture(mode: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "fr-worker-fixture-{}-{n}-{mode}.py",
        std::process::id()
    ));
    std::fs::write(
        &path,
        include_str!("support/worker_fixture.py").replace("@MODE@", mode),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}
fn deadline(cx: &Cx, ms: u64) -> Deadline {
    Deadline::after(cx, Duration::from_millis(ms)).unwrap()
}
async fn worker(cx: &Cx, mode: &str) -> Worker {
    Worker::start(
        cx,
        Launch::new(&fixture(mode), ":0", None, Role::Capture, 77).unwrap(),
        config(),
        deadline(cx, 1000),
    )
    .await
    .unwrap()
}
#[test]
fn real_pipe_exchange_and_acknowledged_stop_reap_the_child() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut w = worker(&cx, "healthy").await;
        assert_eq!(
            w.request(&cx, Kind::Poll, vec![], deadline(&cx, 500))
                .await
                .unwrap()
                .header
                .kind,
            Kind::NeedInput
        );
        w.request(&cx, Kind::Stop, vec![], deadline(&cx, 500))
            .await
            .unwrap();
        assert_eq!(w.state(), State::Stopped);
        assert!(w.reap(&cx, deadline(&cx, 500)).await.unwrap().success());
        assert_eq!(w.state(), State::Reaped);
    });
}
#[test]
fn deadline_and_partial_reply_poison_instead_of_retrying() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for mode in ["stall", "partial"] {
            let mut w = worker(&cx, mode).await;
            assert_eq!(
                w.request(&cx, Kind::Poll, vec![], deadline(&cx, 40))
                    .await
                    .unwrap_err(),
                Error::Deadline
            );
            assert_eq!(w.state(), State::Poisoned);
            assert_eq!(
                w.request(&cx, Kind::Poll, vec![], deadline(&cx, 100))
                    .await
                    .unwrap_err(),
                Error::Unavailable
            );
            assert!(!w.reap(&cx, deadline(&cx, 500)).await.unwrap().success());
        }
    });
}
#[test]
fn cancel_while_waiting_and_dropping_polled_future_both_kill() {
    let rt = runtime();
    let cancel = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let current = Cx::current().unwrap();
        let mut w = worker(&cancel, "stall").await;
        {
            let until = deadline(&cancel, 500);
            let mut request = pin!(w.request(&cancel, Kind::Poll, vec![], until));
            poll_fn(|task| {
                assert!(request.as_mut().poll(task).is_pending());
                Poll::Ready(())
            })
            .await;
            cancel.cancel_fast(CancelKind::User);
            assert_eq!(request.await.unwrap_err(), Error::Cancelled);
        }
        assert_eq!(w.state(), State::Poisoned);
        w.reap(&current, deadline(&current, 500)).await.unwrap();
        let mut w = worker(&current, "stall").await;
        {
            let until = deadline(&current, 500);
            let mut request = pin!(w.request(&current, Kind::Poll, vec![], until));
            poll_fn(|task| {
                assert!(request.as_mut().poll(task).is_pending());
                Poll::Ready(())
            })
            .await;
        }
        assert_eq!(w.state(), State::Poisoned);
        w.reap(&current, deadline(&current, 500)).await.unwrap();
    });
}
#[test]
fn forged_sequence_oversize_and_crash_fail_without_body_allocation_or_retry() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for (mode, expected) in [
            (
                "wrong-sequence",
                Error::Protocol(fr_media::worker::Error::WrongSequence),
            ),
            (
                "oversize",
                Error::Protocol(fr_media::worker::Error::ResourceLimit),
            ),
            ("eof", Error::PeerClosed),
        ] {
            let mut w = worker(&cx, mode).await;
            assert_eq!(
                w.request(&cx, Kind::Poll, vec![], deadline(&cx, 500))
                    .await
                    .unwrap_err(),
                expected
            );
            assert_eq!(w.state(), State::Poisoned);
            w.reap(&cx, deadline(&cx, 500)).await.unwrap();
        }
    });
}
#[test]
fn launch_is_local_only_and_deadlines_cannot_be_unbounded() {
    assert!(
        Launch::new(
            std::path::Path::new("relative"),
            ":0",
            None,
            Role::Capture,
            1
        )
        .is_err()
    );
    assert!(
        Launch::new(
            std::path::Path::new("/worker"),
            "host:0",
            None,
            Role::Capture,
            1
        )
        .is_err()
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        assert!(Deadline::after(&cx, Duration::ZERO).is_err());
        assert!(Deadline::after(&cx, Duration::from_secs(6)).is_err());
        let mut w = worker(&cx, "healthy").await;
        assert_eq!(
            w.request(&cx, Kind::Present, vec![], deadline(&cx, 100))
                .await
                .unwrap_err(),
            Error::Protocol(fr_media::worker::Error::WrongRole)
        );
        assert_eq!(w.state(), State::Running);
        w.abort();
        w.reap(&cx, deadline(&cx, 500)).await.unwrap();
    });
}
