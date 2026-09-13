//! Continuous receiving shares the already-established real control connection.
//! Native effects and codec output here are explicit fixtures, not pixel claims.
use super::*;
#[test]
#[allow(clippy::too_many_lines)]
fn continuous_viewer_keeps_input_and_receipts_live_during_decode() {
    continuous_control(false);
}
#[test]
fn receiver_load_feedback_cannot_block_native_input_and_receipts_during_decode() {
    continuous_control(true);
}
#[allow(clippy::too_many_lines)]
fn continuous_control(feedback: bool) {
    run(|c, h| async move {
        let Fixture {
            mut host,
            mut viewer,
            mut host_clock,
            host_media,
            receiver,
            mut driver,
            effects,
            observation,
            presenter,
            video,
            ..
        } = Box::pin(fixture_with_wire_feedback(&c, &h, caps(), true, feedback)).await;
        let reports = std::cell::Cell::new(0);
        let mut feedback_binding = host_media.binding();
        feedback_binding.parent = viewer.session.opened.binding;
        let feedback_route = Route::Stream(host.io().unwrap().1.inbound);
        let limits = host_media.limits();
        let feedback_outbound = Route::Stream(host.io().unwrap().1.outbound);
        let mut requester = fr_media::receiver_feedback::Requester::new(
            feedback_binding,
            *limits.protocol(),
            now(&h).unwrap(),
        )
        .unwrap();
        let bindings = host_media.bindings();
        let descriptor = FrameDescriptor {
            frame: 1,
            reference: Some(0),
            total_bytes: 4,
            stride: 4,
            capture_micros: now(&h).unwrap(),
        };
        let mut bytes = [0; 1150];
        let n = encode_progress(
            Progress {
                descriptor,
                observed_micros: descriptor.capture_micros,
                observation: SourceObservation::Captured,
                pipeline: PipelineState::Running,
            },
            bindings.for_channel(Channel::MediaConfig),
            &limits,
            &mut bytes,
        )
        .unwrap();
        let progress = Route::Stream(host_media.progress_for_test(host.io().unwrap().0));
        let until = descriptor.capture_micros + 200_000;
        admit_record(&mut host, &mut viewer, &h, progress, &bytes[..n], until).await;
        let n = encode_fragment(
            Fragment {
                descriptor,
                index: 0,
                bytes: b"next",
            },
            bindings.for_channel(Channel::Video),
            &limits,
            &mut bytes,
        )
        .unwrap();
        admit_record(
            &mut host,
            &mut viewer,
            &h,
            Route::Datagram(video),
            &bytes[..n],
            until,
        )
        .await;
        let mut receiving = viewer.into_streaming(presenter.unwrap(), receiver).unwrap();
        let stop = receiving.control();
        let mut nonce_counter = 6000;
        let mut tickets = 9000;
        let results = std::cell::Cell::new(0);
        let mut pressed = false;
        let mut released = false;
        let mut decoded = false;
        let end = now(&c).unwrap() + 400_000;
        let ((a, b), shutdown) = Box::pin(support::both(
            support::both(
                async {
                    loop {
                        let q = host.io().map_err(Error::Session)?.0;
                        host_clock
                            .as_mut()
                            .unwrap()
                            .receive(q, block)
                            .map_err(Error::Clock)?;
                        host_clock
                            .as_mut()
                            .unwrap()
                            .service(q)
                            .map_err(Error::Clock)?;
                        if feedback {
                            let current = now(&h).unwrap();
                            requester.prepare(current).unwrap();
                            if let Some((b, until)) = requester.pending(current).unwrap() {
                                match q.send(&h, feedback_outbound, b, until, || true) {
                                    Ok(()) => requester.queued(current).unwrap(),
                                    Err(fr_transport::quic::Error::Backpressure) => {}
                                    Err(e) => panic!("feedback query {e:?}"),
                                }
                            }
                        }
                        host.drive(
                            Duration::from_millis(1),
                            || nonce(&mut nonce_counter),
                            || {
                                tickets += 1;
                                Some(InputTicketId::from_raw(tickets))
                            },
                            |route, bytes| {
                                if crate::media::receiver_feedback::is_feedback(bytes) {
                                    assert!(feedback);
                                    assert_eq!(route, feedback_route);
                                    assert!(requester.receive(bytes, now(&h).unwrap()).unwrap());
                                    reports.set(reports.get() + 1);
                                    Ok(Disposition::Consumed)
                                } else {
                                    block(route, bytes)
                                }
                            },
                        )
                        .await
                        .map_err(Error::Session)?;
                    }
                    #[allow(unreachable_code)]
                    Ok::<(), Error>(())
                },
                receiving.serve(
                    |input, event| {
                        assert!(
                            now(&c).unwrap() < end,
                            "controlled decoder did not complete"
                        );
                        let viewer = input.expect("existing control owner disappeared");
                        if !pressed {
                            let _ = viewer.action(key(true)).unwrap();
                            pressed = true;
                        }
                        if results.get() >= 1 && !released {
                            let _ = viewer.action(key(false)).unwrap();
                            released = true;
                        }
                        if let Some(event) = event {
                            assert_eq!(event.frame.as_raw(), 1);
                            assert_eq!(
                                results.get(),
                                2,
                                "codec wait blocked actual input receipts"
                            );
                            assert!(released);
                            // The local visibility witness is an explicit test fixture.
                            viewer.visible(1).unwrap();
                            decoded = true;
                            stop.stop();
                            observation.revoke();
                        }
                        Ok(())
                    },
                    |_| {
                        results.set(results.get() + 1);
                    },
                    block,
                ),
            ),
            driver.take().unwrap(),
        ))
        .await;
        assert!(a.is_err() && b.is_err() && decoded);
        assert!(shutdown.handoff_safe());
        assert_eq!(effects.lock().unwrap().keys, [true, false]);
        assert_eq!(receiving.statistics().decoded, 1);
        assert!(receiving.statistics().network_turns >= 3);
        if feedback {
            assert!(reports.get() > 0);
            assert!(receiving.receiver_feedback_reports() > 0);
        } else {
            assert_eq!(reports.get(), 0);
            assert_eq!(receiving.receiver_feedback_reports(), 0);
        }
        receiving
            .reap_media(
                &h,
                crate::worker::Deadline::after(&h, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
    });
}

/// Attachment completion can leave transport bytes pending. Admit the exact
/// prepared record while servicing both original sessions, without sleeping,
/// re-encoding it, resetting its deadline, or replaying an accepted record.
async fn admit_record(
    host: &mut ControlledHost,
    viewer: &mut ControlledViewer,
    cx: &Cx,
    route: Route,
    bytes: &[u8],
    until: u64,
) {
    let mut counter = 5100;
    loop {
        assert!(
            now(cx).unwrap() < until,
            "fixture send exceeded original lifetime"
        );
        match host.io().unwrap().0.send(cx, route, bytes, until, || {
            now(cx).is_ok_and(|current| current < until)
        }) {
            Ok(()) => return,
            Err(quic::Error::Backpressure) => {}
            Err(error) => panic!("fixture send refused: {error:?}"),
        }
        let (host_result, viewer_result) = Box::pin(support::both(
            host.drive(
                Duration::from_millis(1),
                || nonce(&mut counter),
                || Some(InputTicketId::from_raw(6000)),
                block,
            ),
            viewer.drive(Duration::from_millis(1), |_| {}, block),
        ))
        .await;
        host_result.unwrap();
        viewer_result.unwrap();
    }
}
