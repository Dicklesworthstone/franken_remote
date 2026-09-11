//! Continuous receiving shares the already-established real control connection.
//! Native effects and codec output here are explicit fixtures, not pixel claims.
use super::*;
#[test]
#[allow(clippy::too_many_lines)]
fn continuous_viewer_keeps_input_and_receipts_live_during_decode() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            viewer,
            mut host_clock,
            host_media,
            receiver,
            mut driver,
            effects,
            observation,
            presenter,
            video,
            ..
        } = Box::pin(fixture_with_decoder(&c, &h, caps(), true)).await;
        let limits = host_media.limits();
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
        let q = host.io().unwrap().0;
        let progress = Route::Stream(host_media.progress_for_test(q));
        q.send(
            &h,
            progress,
            &bytes[..n],
            now(&h).unwrap() + 200_000,
            || true,
        )
        .unwrap();
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
        q.send(
            &h,
            Route::Datagram(video),
            &bytes[..n],
            now(&h).unwrap() + 200_000,
            || true,
        )
        .unwrap();
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
                        host_clock.receive(q, block).map_err(Error::Clock)?;
                        host_clock.service(q).map_err(Error::Clock)?;
                        host.drive(
                            Duration::from_millis(1),
                            || nonce(&mut nonce_counter),
                            || {
                                tickets += 1;
                                Some(InputTicketId::from_raw(tickets))
                            },
                            block,
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
        receiving
            .reap_media(
                &h,
                crate::worker::Deadline::after(&h, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
    });
}
