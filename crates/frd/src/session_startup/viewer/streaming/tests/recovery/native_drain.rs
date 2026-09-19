//! Actual canonical viewer loop, renewal/control TLS/UDP and supervised decoder
//! process. Compressed bytes and codec completion remain explicit fixtures.
use super::*;
use std::cell::Cell;

fn receive(receiver: &mut ReceivePipeline, cfg: ReceiveConfig, frame: u64, cx: &Cx) {
    let bytes = fragments(
        cfg,
        FrameDescriptor {
            frame,
            reference: Some(frame - 1),
            capture_micros: now(cx).unwrap(),
            total_bytes: 4,
            stride: 4,
        },
    );
    receiver
        .receive(Channel::Video, &bytes, now(cx).unwrap())
        .unwrap();
}

#[test]
#[allow(clippy::too_many_lines)]
fn canonical_observer_drains_inflight_decode_and_keeps_servicing_failed_chain() {
    run(|c, h| async move {
        let (mut host, peer, mut old_receiver, mut old_watcher, _) = Box::pin(pair(&c, &h)).await;
        old_receiver.close();
        old_watcher.close();
        let Peer::Observe { session, media } = peer else {
            panic!("observer fixture")
        };
        let cfg = media
            .receiver_config(
                &session.transport,
                ReceivePolicy {
                    reference_budget_micros: 200_000,
                    recovery_budget_micros: 5_000_000,
                    ..ReceivePolicy::default()
                },
            )
            .unwrap();
        let mut receiver =
            ReceivePipeline::new(cfg, MediaBudget::new(cfg.limits.protocol()).unwrap()).unwrap();
        let mut presenter =
            crate::media::Presenter::stream_fixture(&c, &session.transport, &media, &mut receiver)
                .await;
        let worker_id = presenter.worker_id();
        let mut bytes = [0; 1150];
        let n = encode_recovery(
            RecoveryChunk {
                frame: 0,
                total_bytes: 4,
                offset: 0,
                capture_micros: now(&c).unwrap(),
                bytes: b"fake",
            },
            cfg.bindings.for_channel(Channel::Recovery),
            &cfg.limits,
            &mut bytes,
        )
        .unwrap();
        receiver
            .receive(Channel::Recovery, &bytes[..n], now(&c).unwrap())
            .unwrap();
        let initial = presenter
            .present_next(&c, &mut receiver)
            .await
            .unwrap()
            .unwrap();
        let mut bound = media.binding();
        bound.parent = host.binding();
        let control_in = Route::Stream(host.io().unwrap().1.inbound);
        // Reordered complete dependent picture expires sooner than the missing
        // predecessor, which the native fixture takes 90 ms to decode.
        receive(&mut receiver, cfg, 2, &c);
        asupersync::time::sleep(c.now(), Duration::from_millis(140)).await;
        receive(&mut receiver, cfg, 1, &c);
        let mut viewer =
            StreamingViewer::new(Peer::Observe { session, media }, presenter, receiver).unwrap();
        viewer
            .recovery
            .as_mut()
            .unwrap()
            .observe_decoded(&initial.decoded)
            .unwrap();
        let stop = viewer.control();
        let until = now(&c).unwrap() + 650_000;
        let stopped = Cell::new(false);
        let mut nonce = 10_000_u128;
        let mut reports = 0;
        let mut presentations = 0;
        let (host_result, viewer_result) = Box::pin(support::both(
            async {
                while !stopped.get() {
                    host.drive(
                        Duration::from_millis(2),
                        || {
                            nonce += 1;
                            Ok(nonce)
                        },
                        |route, bytes| {
                            if route == control_in
                                && bytes.get(6..8)
                                    == Some(&(Kind::RecoveryRequest as u16).to_be_bytes())
                            {
                                let request = recovery_request::decode(
                                    bytes,
                                    bound,
                                    &ProtocolLimits::ABSOLUTE,
                                    fr_wire::input::InputDirection::ViewerToHost,
                                    fr_wire::input::InputDelivery::Reliable,
                                )
                                .unwrap();
                                assert_eq!(
                                    request.reason,
                                    recovery_request::Reason::ReferenceExpired
                                );
                                assert_eq!(request.last_useful_frame, Some(0));
                                reports += 1;
                                return Ok(Disposition::Consumed);
                            }
                            block(route, bytes)
                        },
                    )
                    .await?;
                }
                Ok::<(), crate::session_startup::Error>(())
            },
            viewer.serve(
                |controlled, event| {
                    assert!(controlled.is_none());
                    if event.is_some() {
                        presentations += 1;
                    }
                    if now(&c).unwrap() >= until {
                        stopped.set(true);
                        stop.stop();
                    }
                    Ok(())
                },
                |_| {},
                block,
            ),
        ))
        .await;
        // A newer outer coordinator correctly refuses this fixture's original
        // RequestControl offer after the drain. This test does not fabricate a
        // new observation-only offer or qualify replacement-channel startup.
        // On the report-only loop the explicit timer stop ends the same test.
        assert!(
            stopped.get() || matches!(viewer_result, Err(Error::Closed)),
            "native work failed before recovery continuation: {host_result:?} {viewer_result:?}"
        );
        assert!(viewer_result.is_err());
        assert_eq!(reports, 1);
        assert_eq!(
            presentations, 0,
            "obsolete decoder output became a presentation"
        );
        assert_eq!(viewer.statistics().decoded, 0);
        assert_eq!(viewer.statistics().compositor_submissions, 0);
        assert!(viewer.statistics().network_turns > 2);
        assert_eq!(viewer.budget_usage(), BudgetUsage::default());
        assert_eq!(viewer.worker_id(), worker_id);
        host.close();
        let cleanup = Cx::current().unwrap();
        viewer
            .reap_media(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
    });
}
