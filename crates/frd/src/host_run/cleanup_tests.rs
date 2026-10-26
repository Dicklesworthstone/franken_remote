//! Receipt classification with real supervised child custody. `ReapPending`
//! from the source stage is deliberately injected; these tests do not claim
//! that a scripted child reproduces a delayed physical GPU driver's exit.
use super::{SourceRetirement, source_retirement};
use crate::worker::{Deadline, Error, Launch, Retirement, Worker};
use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    types::{Budget, CancelKind, Time},
};
use fr_core::ids::CodecConfigurationGeneration;
use fr_media::worker::{Backend, Configuration, Kind, Role};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
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

fn deadline(cx: &Cx) -> Deadline {
    Deadline::after(cx, Duration::from_secs(1)).unwrap()
}

fn launch() -> (Launch, Retirement) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-host-cleanup-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    crate::executable::write(
        &path,
        include_str!("../../tests/support/worker_fixture.py").replace("@MODE@", "healthy"),
    );
    Launch::new(&path, ":0", None, Role::Capture, 991)
        .unwrap()
        .retain_cleanup()
        .unwrap()
}

async fn worker(cx: &Cx, launch: Launch) -> Worker {
    Worker::start(
        cx,
        launch,
        Configuration {
            width: 320,
            height: 240,
            fps: 30,
            backend: Backend::SoftwareExplicit,
            bitrate: 2_000_000,
            max_access_unit_bytes: 1024 * 1024,
            generation: CodecConfigurationGeneration::INITIAL,
        },
        deadline(cx),
    )
    .await
    .unwrap()
}

#[test]
fn source_timeout_recovers_only_after_the_original_child_exit_receipt() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (launch, mut closing) = launch();
        let mut worker = worker(&cx, launch).await;
        let pid = worker.id().unwrap();
        let pending = closing.reap(&cx, deadline(&cx)).await;
        assert_eq!(pending, Err(Error::ReapPending));
        assert_eq!(
            source_retirement(Err(Error::ReapPending), Some(pending)),
            Err(Error::ReapPending)
        );
        // Custody did not kill or take a still-owned source to produce evidence.
        worker
            .request(&cx, Kind::Poll, vec![], deadline(&cx))
            .await
            .unwrap();
        // This is the same ownership transfer as Driver::release after timeout.
        drop(worker);
        let status = closing.reap(&cx, deadline(&cx)).await.unwrap().unwrap();
        assert!(
            !status.success(),
            "abort is not an acknowledged graceful stop"
        );
        assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
        assert_eq!(
            source_retirement(Err(Error::ReapPending), Some(Ok(Some(status)))),
            Ok(SourceRetirement::Forced)
        );
        assert_eq!(
            source_retirement(Ok(Some(status)), Some(Ok(Some(status)))),
            Ok(SourceRetirement::Complete)
        );
    });
}

#[test]
fn missing_or_never_spawned_custody_cannot_clear_a_source_timeout() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        assert_eq!(
            source_retirement(Err(Error::ReapPending), None),
            Err(Error::ReapPending)
        );
        let (launch, mut closing) = launch();
        // A retained but still-owned launch is not proof that nothing started.
        let pending = closing.reap(&cx, deadline(&cx)).await;
        assert_eq!(pending, Err(Error::ReapPending));
        assert_eq!(
            source_retirement(Err(Error::ReapPending), Some(pending)),
            Err(Error::ReapPending)
        );
        drop(launch);
        let never_spawned = closing.reap(&cx, deadline(&cx)).await;
        assert_eq!(never_spawned, Ok(None));
        assert_eq!(
            source_retirement(Err(Error::ReapPending), Some(never_spawned)),
            Err(Error::ReapPending)
        );
        assert_eq!(
            source_retirement(Ok(None), Some(never_spawned)),
            Ok(SourceRetirement::Complete)
        );
    });
}

#[test]
fn failed_receipt_collection_stays_fatal_even_after_the_worker_was_aborted() {
    let runtime = runtime();
    let cancelled = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let cx = Cx::current().unwrap();
        let (launch, mut closing) = launch();
        let worker = worker(&cx, launch).await;
        let pid = worker.id().unwrap();
        drop(worker);
        let expired = deadline(&cx).capped_at(Time::ZERO);
        let unconfirmed = closing.reap(&cx, expired).await;
        assert_eq!(unconfirmed, Err(Error::ReapPending));
        assert_eq!(
            source_retirement(Err(Error::ReapPending), Some(unconfirmed)),
            Err(Error::ReapPending)
        );
        cancelled.cancel_fast(CancelKind::User);
        let unconfirmed = closing.reap(&cancelled, deadline(&cx)).await;
        assert_eq!(unconfirmed, Err(Error::Cancelled));
        assert_eq!(
            source_retirement(Err(Error::ReapPending), Some(unconfirmed)),
            Err(Error::ReapPending)
        );
        // A separate bounded cleanup retains the exact original child; neither
        // the failed receipt nor the source classification discarded custody.
        let confirmed = closing.reap(&cx, deadline(&cx)).await;
        assert!(matches!(confirmed, Ok(Some(_))));
        assert_eq!(
            source_retirement(Err(Error::ReapPending), Some(confirmed)),
            Ok(SourceRetirement::Forced)
        );
        assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
    });
}

#[test]
fn positive_exit_receipt_preserves_every_other_source_error() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (launch, mut closing) = launch();
        drop(worker(&cx, launch).await);
        let confirmed = closing.reap(&cx, deadline(&cx)).await;
        assert!(matches!(confirmed, Ok(Some(_))));
        for failure in [
            Error::MissingRuntime,
            Error::InvalidLaunch,
            Error::Cancelled,
            Error::Deadline,
            Error::ClockRegression,
            Error::SpawnFailed(crate::worker::SpawnFailure::Unsupported),
            Error::PipeFailed,
            Error::PeerClosed,
            Error::Protocol(fr_media::worker::Error::WrongState),
            Error::WorkerRefused(fr_media::worker::Error::WrongState),
            Error::Unavailable,
        ] {
            assert_eq!(
                source_retirement(Err(failure), Some(confirmed)),
                Err(failure),
                "only ReapPending may be resolved by later positive exit evidence"
            );
        }
    });
}
