//! Actual supervised processes and pipes, not native codec qualification.
use super::*;
use frd::worker::Retirement;

fn launch(mode: &str) -> (Launch, Retirement) {
    Launch::new(&fixture(mode), ":0", None, Role::Capture, 991)
        .unwrap()
        .retain_cleanup()
        .unwrap()
}

#[test]
fn unpolled_and_failed_spawn_have_positive_no_child_receipts() {
    run_worker!(cx, {
        let (launch, mut closing) = launch("healthy");
        assert_eq!(
            closing.reap(&cx, deadline(&cx, 500)).await,
            Err(Error::ReapPending)
        );
        drop(Worker::start(&cx, launch, config(), deadline(&cx, 500)));
        assert_eq!(closing.reap(&cx, deadline(&cx, 500)).await, Ok(None));
        assert_eq!(closing.reap(&cx, deadline(&cx, 500)).await, Ok(None));

        let absent = std::env::temp_dir()
            .join(format!("fr-missing-worker-{}", std::process::id()))
            .join("does-not-exist");
        let (launch, mut closing) = Launch::new(&absent, ":0", None, Role::Present, 77)
            .unwrap()
            .retain_cleanup()
            .unwrap();
        assert!(matches!(
            Worker::start(&cx, launch, config(), deadline(&cx, 500)).await,
            Err(Error::SpawnFailed(_))
        ));
        assert_eq!(closing.reap(&cx, deadline(&cx, 500)).await, Ok(None));
    });
}

#[test]
fn failed_and_timed_out_configuration_leave_the_actual_child_collectable() {
    run_worker!(cx, {
        for mode in ["exit-configure", "stall-configure"] {
            let (launch, mut closing) = launch(mode);
            let result = Worker::start(&cx, launch, config(), deadline(&cx, 80)).await;
            assert!(matches!(result, Err(Error::PeerClosed | Error::Deadline)));
            let status = closing
                .reap(&cx, deadline(&cx, 1000))
                .await
                .unwrap()
                .unwrap();
            assert!(!status.success());
            assert_eq!(
                closing.reap(&cx, deadline(&cx, 500)).await,
                Ok(Some(status))
            );
        }
    });
}

#[test]
fn dropped_polled_startup_retains_custody_and_does_not_kill_another_worker() {
    run_worker!(cx, {
        let mut foreign = worker(&cx, "healthy").await;
        let (launch, mut closing) = launch("stall-configure");
        {
            let mut starting = pin!(Worker::start(&cx, launch, config(), deadline(&cx, 1000)));
            poll_fn(|task| {
                assert!(starting.as_mut().poll(task).is_pending());
                Poll::Ready(())
            })
            .await;
        }
        let status = closing
            .reap(&cx, deadline(&cx, 1000))
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
        assert_eq!(
            foreign
                .request(&cx, Kind::Poll, vec![], deadline(&cx, 500))
                .await
                .unwrap()
                .header
                .kind,
            Kind::NeedInput
        );
        foreign.abort();
        foreign.reap(&cx, deadline(&cx, 500)).await.unwrap();
    });
}

#[test]
fn live_worker_cannot_be_reaped_via_custody_and_drop_retains_its_exact_exit() {
    run_worker!(cx, {
        let (launch, mut closing) = launch("healthy");
        let mut worker = Worker::start(&cx, launch, config(), deadline(&cx, 1000))
            .await
            .unwrap();
        let pid = worker.id().unwrap();
        assert_eq!(
            closing.reap(&cx, deadline(&cx, 100)).await,
            Err(Error::ReapPending)
        );
        assert_eq!(worker.state(), State::Running);
        worker
            .request(&cx, Kind::Poll, vec![], deadline(&cx, 500))
            .await
            .unwrap();
        drop(worker);
        assert!(
            !closing
                .reap(&cx, deadline(&cx, 1000))
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
    });
}

#[test]
fn ordinary_acknowledged_stop_and_reap_publish_the_same_receipt_to_custody() {
    run_worker!(cx, {
        let (launch, mut closing) = launch("healthy");
        let mut worker = Worker::start(&cx, launch, config(), deadline(&cx, 1000))
            .await
            .unwrap();
        worker
            .request(&cx, Kind::Stop, vec![], deadline(&cx, 500))
            .await
            .unwrap();
        let status = worker.reap(&cx, deadline(&cx, 1000)).await.unwrap();
        assert!(status.success());
        assert_eq!(
            closing.reap(&cx, deadline(&cx, 500)).await,
            Ok(Some(status))
        );
        drop(worker);
        assert_eq!(
            closing.reap(&cx, deadline(&cx, 500)).await,
            Ok(Some(status))
        );
    });
}

#[test]
fn cancelled_and_expired_cleanup_do_not_discard_the_original_child() {
    let rt = runtime();
    let cancelled = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let (launch, mut closing) = launch("healthy");
        let worker = Worker::start(&cx, launch, config(), deadline(&cx, 1000))
            .await
            .unwrap();
        let pid = worker.id().unwrap();
        drop(worker);
        let until = deadline(&cx, 500);
        cancelled.cancel_fast(CancelKind::User);
        assert_eq!(closing.reap(&cancelled, until).await, Err(Error::Cancelled));
        let elapsed = until.capped_at(asupersync::types::Time::ZERO);
        assert_eq!(closing.reap(&cx, elapsed).await, Err(Error::ReapPending));
        drop(closing.reap(&cx, until));
        assert!(closing.reap(&cx, until).await.unwrap().is_some());
        assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
    });
}
