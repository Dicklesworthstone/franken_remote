//! Scheduler/staging expiry before native submission, using the real supervised
//! worker and synthetic codec from the existing fixture. No live-load claim.
use super::*;

async fn reap(presenter: &mut Presenter, cx: &Cx) {
    presenter.abort();
    presenter
        .reap(cx, Deadline::after(cx, Duration::from_secs(1)).unwrap())
        .await
        .unwrap();
}

#[test]
fn unstarted_expired_picture_keeps_worker_for_the_original_recovery() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, mut receiver, limits) = short_reference(&cx).await;
        let worker = presenter.worker_id();
        let job = presenter.take_next(&cx, &mut receiver).unwrap().unwrap();
        let until = job.picture().reference_deadline_us();
        let charged = receiver.budget_usage();
        // Constructed with valid ownership but not polled until after a
        // scheduler stall. There has been NO native submission for this job.
        let pending = presenter.decode_stream_job(&cx, job, true);
        sleep(cx.now(), Duration::from_millis(65)).await;
        assert!(host_now(&cx).unwrap().as_micros() >= until);
        let retired = pending.await.unwrap();
        assert!(retired.stage.is_none(), "no native completion may be invented");
        assert_eq!(retired.picture().reference_deadline_us(), until);
        assert_eq!(receiver.budget_usage(), charged);
        assert_eq!(presenter.worker_id(), worker);
        assert_eq!(presenter.worker.state(), worker::State::Running);

        // The canonical loop ticks the ORIGINAL receiver before collecting a
        // job. This establishes recovery, not an uncorrelated decoder error.
        assert_eq!(
            receiver.tick(host_now(&cx).unwrap().as_micros()),
            Err(DeliveryError::ReferenceExpired)
        );
        assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
        assert_eq!(receiver.budget_usage(), charged);
        assert!(matches!(
            retired.complete(&cx, &mut receiver),
            Err(Error::InvalidFrame)
        ));
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());

        let epoch = MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::from_raw(2),
        };
        let bindings = MediaBindings::new(5, 6, 7, 8).unwrap();
        presenter
            .recover(&cx, &mut receiver, epoch, bindings)
            .unwrap();
        let mut bytes = [0; 1150];
        let n = encode_recovery(
            RecoveryChunk {
                frame: 2,
                total_bytes: 4,
                offset: 0,
                capture_micros: 7,
                bytes: b"idr2",
            },
            bindings.for_channel(Channel::Recovery),
            &limits,
            &mut bytes,
        )
        .unwrap();
        receiver
            .receive(
                Channel::Recovery,
                &bytes[..n],
                host_now(&cx).unwrap().as_micros(),
            )
            .unwrap();
        let receipt = presenter
            .present_next(&cx, &mut receiver)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.frame, FrameId::from_raw(2));
        assert_eq!(receipt.decoded.epoch(), epoch);
        assert_eq!(receiver.state(), ReceiveState::Streaming);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
        assert_eq!(presenter.worker_id(), worker);
        reap(&mut presenter, &cx).await;
    });
}

#[test]
fn preflight_expiry_remains_terminal_without_recovery_negotiation() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, mut receiver, _) = short_reference(&cx).await;
        let job = presenter.take_next(&cx, &mut receiver).unwrap().unwrap();
        let pending = presenter.decode_job(&cx, job);
        sleep(cx.now(), Duration::from_millis(65)).await;
        assert!(matches!(
            pending.await,
            Err(Error::Worker(worker::Error::Deadline))
        ));
        assert_eq!(presenter.worker.state(), worker::State::Poisoned);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
        assert!(receiver.tick(host_now(&cx).unwrap().as_micros()).is_err());
        assert_eq!(receiver.state(), ReceiveState::Closed);
        reap(&mut presenter, &cx).await;
    });
}

#[test]
fn staging_gate_keeps_exact_deadline_and_checks_fencing_before_expiry() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, mut receiver, _) = short_reference(&cx).await;
        let job = presenter.take_next(&cx, &mut receiver).unwrap().unwrap();
        let until = job.picture().reference_deadline_us();
        let charged = receiver.budget_usage();
        // The two production gates share one rule. The second clock models
        // time spent staging an AU; no allocator timing assumption is needed.
        assert_eq!(job.may_submit(&cx, until - 1, true), Ok(true));
        assert_eq!(job.may_submit(&cx, until, true), Ok(false));
        assert_eq!(job.may_submit(&cx, until + 1, true), Ok(false));
        assert_eq!(
            job.may_submit(&cx, until, false),
            Err(Error::Worker(worker::Error::Deadline))
        );
        assert_eq!(receiver.budget_usage(), charged);
        receiver.close();
        assert_eq!(
            job.may_submit(&cx, until, true),
            Err(Error::Receiver(DeliveryError::DecodeMismatch))
        );
        assert_eq!(receiver.budget_usage(), charged);
        drop(job);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
        reap(&mut presenter, &cx).await;
    });
}

#[test]
fn cancelled_request_cannot_turn_expiry_into_optional_retirement() {
    let rt = runtime();
    let request = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    rt.block_on(async {
        let cleanup = Cx::current().unwrap();
        let (mut presenter, mut receiver, _) = short_reference(&request).await;
        let job = presenter.take_next(&request, &mut receiver).unwrap().unwrap();
        let pending = presenter.decode_stream_job(&request, job, true);
        sleep(cleanup.now(), Duration::from_millis(65)).await;
        request.cancel_fast(asupersync::types::CancelKind::User);
        assert!(matches!(
            pending.await,
            Err(Error::Worker(worker::Error::Cancelled))
        ));
        assert_eq!(presenter.worker.state(), worker::State::Poisoned);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
        assert!(receiver.tick(host_now(&cleanup).unwrap().as_micros()).is_err());
        assert_eq!(receiver.state(), ReceiveState::Closed);
        reap(&mut presenter, &cleanup).await;
    });
}

#[test]
fn abandoning_an_expired_but_unpolled_operation_still_aborts_its_worker() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, mut receiver, _) = short_reference(&cx).await;
        let job = presenter.take_next(&cx, &mut receiver).unwrap().unwrap();
        sleep(cx.now(), Duration::from_millis(65)).await;
        // Only a polled, positively retired preflight may disarm the guard.
        drop(presenter.decode_stream_job(&cx, job, true));
        assert_eq!(presenter.worker.state(), worker::State::Poisoned);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
        assert!(receiver.tick(host_now(&cx).unwrap().as_micros()).is_err());
        assert_eq!(receiver.state(), ReceiveState::Closed);
        reap(&mut presenter, &cx).await;
    });
}
