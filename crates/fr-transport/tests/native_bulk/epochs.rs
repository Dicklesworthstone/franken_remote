//! Real TLS/UDP: continuous records must not inherit an acknowledged deadline.
use super::*;

fn progress(sequence: u64) -> Vec<u8> {
    let mut bytes = vec![0; 1150];
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 65_536, 16_384, 64).unwrap();
    let n = encode_progress(
        Progress {
            descriptor: FrameDescriptor {
                frame: sequence,
                total_bytes: 100,
                stride: 100,
                capture_micros: sequence + 1,
                reference: None,
            },
            observed_micros: sequence + 1,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        3,
        &limits,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}

#[test]
fn continuous_progress_retires_acked_epochs_with_newer_records_still_queued() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx, Policy::default()).await;
        let ending = clock(&cx) + 1_500_000;
        let (mut sent, mut received) = (0, 0);
        let mut pending = None;
        while clock(&cx) < ending || pending.is_some() {
            let (bytes, deadline) =
                pending.get_or_insert_with(|| (progress(sent), clock(&cx) + 250_000));
            // Backpressure is refusal, not admission. A retry of that exact
            // pending record retains its original deadline and sequence.
            match pair.server.send(
                &cx,
                Route::Stream(pair.host_routes[0]),
                bytes,
                *deadline,
                || true,
            ) {
                Ok(()) => {
                    sent += 1;
                    pending = None;
                }
                Err(Error::Backpressure) => {}
                Err(error) => panic!("continuous stream: {error:?}"),
            }
            drive(&cx, &mut pair).await;
            pair.client
                .receive(
                    &cx,
                    || true,
                    |_, bytes| {
                        assert_eq!(bytes, progress(received));
                        received += 1;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            let usage = pair.server.usage();
            assert!(usage.critical_send_records <= Policy::default().critical_send_records);
            assert!(usage.critical_send_bytes <= Policy::default().critical_send_bytes);
        }
        let until = clock(&cx) + 250_000;
        while pair.server.usage().retained_send_records != 0 {
            assert!(clock(&cx) < until);
            drive(&cx, &mut pair).await;
            pair.client
                .receive(
                    &cx,
                    || true,
                    |_, bytes| {
                        assert_eq!(bytes, progress(received));
                        received += 1;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        assert!(sent > 10, "exercise successive native epochs");
        assert_eq!(received, sent);
        assert!(!pair.server.is_closed());
    });
}

#[test]
fn newer_epoch_cannot_extend_unacknowledged_or_waiting_record_deadlines() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for waiting_expires_first in [false, true] {
            let mut pair = pair(&cx, Policy::default()).await;
            let now = clock(&cx);
            let (first, next) = if waiting_expires_first {
                (1_000_000, 40_000)
            } else {
                (40_000, 1_000_000)
            };
            pair.server
                .send(
                    &cx,
                    Route::Stream(pair.host_routes[0]),
                    &progress(0),
                    now + first,
                    || true,
                )
                .unwrap();
            // Freeze the original epoch with no client ACK service.
            pair.server
                .drive(&cx, Duration::ZERO, || true)
                .await
                .unwrap();
            pair.server
                .send(
                    &cx,
                    Route::Stream(pair.host_routes[0]),
                    &progress(1),
                    now + next,
                    || true,
                )
                .unwrap();
            assert_eq!(pair.server.usage().retained_send_records, 2);
            asupersync::time::sleep(cx.now(), Duration::from_millis(45)).await;
            assert_eq!(pair.server.tick(&cx, || true), Err(Error::Expired));
            assert!(pair.server.is_closed());
            assert_eq!(pair.server.usage().retained_send_records, 0);
        }
    });
}

#[test]
fn later_records_wait_for_the_closed_native_epoch_without_losing_order_or_credit() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx, Policy::default()).await;
        for sequence in 0..2 {
            pair.server
                .send(
                    &cx,
                    Route::Stream(pair.host_routes[0]),
                    &progress(sequence),
                    clock(&cx) + 1_000_000,
                    || true,
                )
                .unwrap();
            pair.server
                .drive(&cx, Duration::ZERO, || true)
                .await
                .unwrap();
        }
        assert_eq!(pair.server.usage().retained_send_records, 2);
        let mut received = 0;
        // Do not give the sender another opportunity to collect acknowledgement.
        // The later record stays accounted in the project queue, not appended
        // to the native retention set whose old deadline needs to be retired.
        for _ in 0..8 {
            pair.client
                .drive(&cx, Duration::from_millis(1), || true)
                .await
                .unwrap();
            pair.client
                .receive(
                    &cx,
                    || true,
                    |_, bytes| {
                        assert_eq!(bytes, progress(received));
                        received += 1;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        assert_eq!(
            received, 1,
            "new admissions cannot join an already staged epoch"
        );
        let until = clock(&cx) + 500_000;
        while pair.server.usage().retained_send_records != 0 {
            assert!(clock(&cx) < until);
            drive(&cx, &mut pair).await;
            pair.client
                .receive(
                    &cx,
                    || true,
                    |_, bytes| {
                        assert_eq!(bytes, progress(received));
                        received += 1;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        assert_eq!(received, 2);
        assert_eq!(pair.server.usage().critical_send_bytes, 0);
        assert_eq!(pair.server.usage().critical_send_records, 0);
    });
}
