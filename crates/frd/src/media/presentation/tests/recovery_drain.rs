//! A native reply is drained under its own existing deadline, not evidence that
//! a fenced reference chain became usable again. Real supervised IPC; fake codec.
use super::*;

async fn reordered(cx: &Cx) -> (Presenter, ReceivePipeline, MediaLimits) {
    let (mut presenter, mut receiver, limits) = fixture_with_policy(
        cx,
        "drain",
        ReceivePolicy {
            reference_budget_micros: 200_000,
            ..ReceivePolicy::default()
        },
    )
    .await;
    let initial = presenter.take_next(cx, &mut receiver).unwrap().unwrap();
    presenter
        .decode_job(cx, initial)
        .await
        .unwrap()
        .complete(cx, &mut receiver)
        .unwrap();
    // A later dependent picture arrives BEFORE the picture it needs. Its
    // reference deadline can expire while that later-arriving picture decodes.
    let mut bytes = [0; 1150];
    let n = encode_fragment(
        Fragment {
            descriptor: FrameDescriptor {
                frame: 2,
                reference: Some(1),
                capture_micros: 7,
                total_bytes: 4,
                stride: 4,
            },
            index: 0,
            bytes: b"tail",
        },
        1,
        &limits,
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(
            Channel::Video,
            &bytes[..n],
            host_now(cx).unwrap().as_micros(),
        )
        .unwrap();
    sleep(cx.now(), Duration::from_millis(140)).await;
    next_frame(cx, &mut receiver, limits);
    (presenter, receiver, limits)
}

#[test]
fn observation_drain_preserves_worker_and_charges_until_native_reply_is_retired() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, mut receiver, _) = reordered(&cx).await;
        let worker = presenter.worker_id();
        let job = presenter.take_next(&cx, &mut receiver).unwrap().unwrap();
        assert_eq!(job.picture().descriptor().frame, 1);
        let original_deadline = job.picture().reference_deadline_us();
        let done = {
            let mut future = pin!(presenter.decode_stream_job(&cx, job, true));
            assert!(poll_once(future.as_mut()).await);
            sleep(cx.now(), Duration::from_millis(70)).await;
            assert_eq!(
                receiver.tick(host_now(&cx).unwrap().as_micros()),
                Err(DeliveryError::ReferenceExpired)
            );
            assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
            // Queued tail retired; the exact native borrow remains charged.
            assert_eq!(receiver.budget_usage().pictures, 1);
            future.await.unwrap()
        };
        assert!(host_now(&cx).unwrap().as_micros() < original_deadline);
        assert_eq!(presenter.worker.state(), worker::State::Running);
        assert_eq!(presenter.worker_id(), worker);
        assert_eq!(receiver.budget_usage().pictures, 1);
        assert!(presenter.take_next(&cx, &mut receiver).is_err());
        drop(done); // NOT complete_decode: there is no usable presentation.
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
        assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
        assert_eq!(presenter.worker.state(), worker::State::Running);
        presenter.abort();
        presenter
            .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

#[test]
fn strict_decoder_still_fails_fast_when_reordered_reference_expires() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, mut receiver, _) = reordered(&cx).await;
        let job = presenter.take_next(&cx, &mut receiver).unwrap().unwrap();
        {
            let mut future = pin!(presenter.decode_job(&cx, job));
            assert!(poll_once(future.as_mut()).await);
            sleep(cx.now(), Duration::from_millis(70)).await;
            assert_eq!(
                receiver.tick(host_now(&cx).unwrap().as_micros()),
                Err(DeliveryError::ReferenceExpired)
            );
            assert!(matches!(
                future.await,
                Err(Error::Receiver(DeliveryError::DecodeMismatch))
            ));
        }
        assert_eq!(presenter.worker.state(), worker::State::Poisoned);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
        presenter
            .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

#[test]
fn drain_does_not_hide_invalid_native_reply_or_extend_native_timeout() {
    for mode in ["bad-reply", "stall"] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let (mut presenter, mut receiver, _) = fixture(&cx, mode).await;
            let job = presenter.take_next(&cx, &mut receiver).unwrap().unwrap();
            let start = std::time::Instant::now();
            let result = presenter.decode_stream_job(&cx, job, true).await;
            match mode {
                "bad-reply" => assert!(matches!(result, Err(Error::InvalidFrame))),
                _ => assert!(matches!(
                    result,
                    Err(Error::Worker(worker::Error::Deadline))
                )),
            }
            assert!(start.elapsed() < Duration::from_secs(1));
            assert_eq!(presenter.worker.state(), worker::State::Poisoned);
            assert_eq!(receiver.budget_usage(), BudgetUsage::default());
            assert!(receiver.tick(host_now(&cx).unwrap().as_micros()).is_err());
            presenter
                .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
                .await
                .unwrap();
        });
    }
}

#[test]
fn abandoning_drain_before_or_after_poll_fences_receiver_and_reaps_original_worker() {
    for polled in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let (mut presenter, mut receiver, _) = fixture(&cx, "stall").await;
            let job = presenter.take_next(&cx, &mut receiver).unwrap().unwrap();
            {
                let mut future = pin!(presenter.decode_stream_job(&cx, job, true));
                if polled {
                    assert!(poll_once(future.as_mut()).await);
                }
            }
            assert_eq!(presenter.worker.state(), worker::State::Poisoned);
            assert!(receiver.tick(host_now(&cx).unwrap().as_micros()).is_err());
            assert_eq!(receiver.state(), ReceiveState::Closed);
            assert_eq!(receiver.budget_usage(), BudgetUsage::default());
            presenter
                .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
                .await
                .unwrap();
        });
    }
}

#[test]
fn drain_cannot_start_native_work_after_an_unpolled_scope_was_fenced() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, mut receiver, _) = fixture(&cx, "stall").await;
        let job = presenter.take_next(&cx, &mut receiver).unwrap().unwrap();
        let future = presenter.decode_stream_job(&cx, job, true);
        receiver.close();
        assert!(matches!(
            future.await,
            Err(Error::Receiver(DeliveryError::DecodeMismatch))
        ));
        assert_eq!(presenter.worker.state(), worker::State::Poisoned);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
        presenter
            .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}
