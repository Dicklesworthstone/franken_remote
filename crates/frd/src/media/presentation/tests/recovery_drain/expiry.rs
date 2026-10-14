//! Real supervised IPC with the existing synthetic 90 ms decoder. A picture's
//! own reference expiry is distinct from the hung-worker watchdog; no hardware
//! throughput or visible-presentation qualification is implied by these tests.
use super::*;

async fn short_reference(cx: &Cx) -> (Presenter, ReceivePipeline, MediaLimits) {
    let (mut presenter, mut receiver, limits) = fixture_with_policy(
        cx,
        "drain",
        ReceivePolicy {
            display_budget_micros: 20_000,
            reference_budget_micros: 50_000,
            repair_interval_micros: 30_000,
            ..ReceivePolicy::default()
        },
    )
    .await;
    // Reliable startup has its separate recovery budget. The following
    // datagram picture, not this IDR, exceeds its reference deadline.
    presenter
        .present_next(cx, &mut receiver)
        .await
        .unwrap()
        .unwrap();
    next_frame(cx, &mut receiver, limits);
    (presenter, receiver, limits)
}

#[test]
fn own_reference_expiry_retires_late_reply_and_recovers_on_the_same_worker() {
    for tick_during_decode in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let (mut presenter, mut receiver, limits) = short_reference(&cx).await;
            let worker_id = presenter.worker_id();
            let job = presenter.take_next(&cx, &mut receiver).unwrap().unwrap();
            let reference_until = job.picture().reference_deadline_us();
            let charged = receiver.budget_usage();
            assert_eq!(charged.pictures, 1);
            let done = {
                let mut future = pin!(presenter.decode_stream_job(&cx, job, true));
                assert!(poll_once(future.as_mut()).await);
                if tick_during_decode {
                    sleep(cx.now(), Duration::from_millis(65)).await;
                    assert_eq!(
                        receiver.tick(host_now(&cx).unwrap().as_micros()),
                        Err(DeliveryError::ReferenceExpired)
                    );
                    assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
                    assert_eq!(receiver.budget_usage(), charged);
                }
                future.await.unwrap()
            };
            assert!(host_now(&cx).unwrap().as_micros() >= reference_until);
            assert_eq!(done.picture().reference_deadline_us(), reference_until);
            assert_eq!(receiver.budget_usage(), charged);
            assert_eq!(presenter.worker.state(), worker::State::Running);
            assert_eq!(presenter.worker_id(), worker_id);
            // Even a caller that attempts completion instead of discarding the
            // obsolete reply cannot produce a DecodedFrame or fresh-view proof.
            let expected = if tick_during_decode {
                DeliveryError::StaleGeneration
            } else {
                DeliveryError::ReferenceExpired
            };
            assert!(matches!(
                done.complete(&cx, &mut receiver),
                Err(Error::Receiver(error)) if error == expected
            ));
            assert_eq!(receiver.budget_usage(), BudgetUsage::default());
            assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
            assert!(presenter.take_next(&cx, &mut receiver).is_err());

            // The normal recovery API can now reuse the SAME decoder owner.
            // Only the new binding/generation's real reply enables streaming.
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
            assert_eq!(presenter.worker_id(), worker_id);
            presenter.abort();
            presenter
                .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
                .await
                .unwrap();
        });
    }
}

#[test]
fn own_reference_expiry_is_still_terminal_without_negotiated_recovery() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, mut receiver, _) = short_reference(&cx).await;
        let job = presenter.take_next(&cx, &mut receiver).unwrap().unwrap();
        assert!(matches!(
            presenter.decode_job(&cx, job).await,
            Err(Error::Worker(worker::Error::Deadline))
        ));
        assert_eq!(presenter.worker.state(), worker::State::Poisoned);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
        assert!(receiver.tick(host_now(&cx).unwrap().as_micros()).is_err());
        presenter
            .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}
