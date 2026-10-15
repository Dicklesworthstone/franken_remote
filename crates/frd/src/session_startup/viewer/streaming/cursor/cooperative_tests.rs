//! Actual authenticated QUIC, canonical network service and supervised presenter
//! IPC. The child sleeps instead of rendering; this is not a live GPU claim.
use super::super::recovery_control;
use super::*;
use crate::{
    media::Presenter,
    session_startup::{
        running::{
            controlled::tests::attach,
            tests::{pair_initialized, run},
        },
        tests::support,
    },
    worker::Deadline,
};
use fr_media::delivery::{
    DeliveryError, MediaBudget, ReceivePipeline, ReceivePolicy, ReceiveState,
};
use fr_wire::{
    Channel, FrameDescriptor, PipelineState, Progress, RecoveryChunk, SourceObservation,
    attachment, decoder, encode_progress, encode_recovery, negotiation::Capability,
    recovery_request,
};
use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

fn position(lanes: CursorLanes, x: i32, sequence: u64) -> Vec<u8> {
    let mut bytes = vec![0; wire::CURSOR_POSITION_RECORD_BYTES];
    let n = wire::encode_cursor_position(
        &wire::CursorPosition {
            shape_id: 5,
            x,
            y: 30,
            geometry_generation: lanes.view.geometry.as_raw(),
            sequence,
            flags: wire::POSITION_FLAG_VISIBLE,
        },
        lanes.position.binding,
        lanes.position_maximum,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}
fn install(cursor: &mut ViewerCursor) {
    let mut bytes = [0; 1150];
    let n = wire::encode_cursor_shape(
        &wire::CursorShape {
            shape_id: 5,
            width: 1,
            height: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            scale_1000: 1000,
            flags: wire::SHAPE_FLAG_VISIBLE,
            rgba: &[1, 2, 3, 255],
        },
        cursor.lanes.shape.binding,
        &cursor.lanes.limits,
        &mut bytes,
    )
    .unwrap();
    cursor
        .receive(Route::Stream(cursor.lanes.shape), &bytes[..n])
        .unwrap();
    cursor
        .receive(
            Route::Datagram(cursor.lanes.position),
            &position(cursor.lanes, 20, 1),
        )
        .unwrap();
}

#[test]
fn redraw_services_actual_quic_and_coalesces_new_cursor_positions() {
    scenario(false);
}
#[test]
fn redraw_services_reference_expiry_and_the_original_recovery_request() {
    scenario(true);
}

#[allow(clippy::too_many_lines)]
fn scenario(lose_picture: bool) {
    run(move |c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut capabilities: Vec<_> = [
            decoder::CAPABILITY,
            attachment::CAPABILITY,
            attachment::DELIVERY_CAPABILITY,
            wire::CAPABILITY,
        ]
        .into_iter()
        .map(|name| Capability {
            name: name.into(),
            version: 1,
            required: true,
        })
        .collect();
        if lose_picture {
            capabilities.push(Capability {
                name: recovery_request::CAPABILITY.into(),
                version: recovery_request::VERSION,
                required: true,
            });
        }
        let (mut host, mut viewer) = pair_initialized(&c, &h, capabilities, |_| {}).await;
        let (hc, vc) = attach(
            &mut host,
            &mut viewer,
            &c,
            &h,
            attachment::MediaRole::Configuration,
            18,
        )
        .await;
        let (hr, vr) = attach(
            &mut host,
            &mut viewer,
            &c,
            &h,
            attachment::MediaRole::Recovery,
            19,
        )
        .await;
        let (hv, vv) = attach(
            &mut host,
            &mut viewer,
            &c,
            &h,
            attachment::MediaRole::Video,
            20,
        )
        .await;
        let selection = host.selection().clone();
        let hm = NegotiatedMedia::new(host.io().unwrap().0, &selection, &hc, &hr, &hv).unwrap();
        let vm = NegotiatedMedia::new(viewer.io().unwrap().0, &selection, &vc, &vr, &vv).unwrap();
        let host_lanes = hm.cursor_lanes(host.io().unwrap().0).unwrap().unwrap();
        let policy = ReceivePolicy {
            display_budget_micros: 20_000,
            reference_budget_micros: 50_000,
            repair_interval_micros: 30_000,
            ..ReceivePolicy::default()
        };
        let cfg = vm
            .receiver_config(viewer.io().unwrap().0, policy)
            .unwrap();
        let mut receiver =
            ReceivePipeline::new(cfg, MediaBudget::new(cfg.limits.protocol()).unwrap()).unwrap();
        let mut presenter = Presenter::stream_fixture_decoding(
            &c,
            viewer.io().unwrap().0,
            &vm,
            &mut receiver,
            "0.15",
        )
        .await;
        let mut bytes = [0; 1150];
        let n = encode_recovery(
            RecoveryChunk {
                frame: 0,
                total_bytes: 4,
                offset: 0,
                capture_micros: 0,
                bytes: b"fake",
            },
            cfg.bindings.for_channel(Channel::Recovery),
            &cfg.limits,
            &mut bytes,
        )
        .unwrap();
        receiver
            .receive(
                Channel::Recovery,
                &bytes[..n],
                super::super::now(&c).unwrap(),
            )
            .unwrap();
        let initial = presenter
            .present_next(&c, &mut receiver)
            .await
            .unwrap()
            .unwrap();
        let mut stream = StreamingViewer::from_test_parts(viewer, vm, presenter, receiver, initial);
        install(stream.cursor.as_mut().unwrap());
        let worker = stream.worker_id();
        let before = stream.statistics();
        if lose_picture {
            let n = encode_progress(
                Progress {
                    descriptor: FrameDescriptor {
                        frame: 1,
                        reference: Some(0),
                        total_bytes: 4,
                        stride: 4,
                        capture_micros: 0,
                    },
                    observed_micros: 0,
                    observation: SourceObservation::Captured,
                    pipeline: PipelineState::Running,
                },
                cfg.bindings.for_channel(Channel::MediaConfig),
                &cfg.limits,
                &mut bytes,
            )
            .unwrap();
            stream
                .receiver
                .receive(
                    Channel::MediaConfig,
                    &bytes[..n],
                    super::super::now(&c).unwrap(),
                )
                .unwrap();
        }
        // Queued only on the host. The original redraw snapshots x=20 before
        // the viewer's new cooperative turn receives this replacement at x=90.
        host.io()
            .unwrap()
            .0
            .send(
                &h,
                Route::Datagram(host_lanes.position),
                &position(host_lanes, 90, 2),
                super::super::now(&h).unwrap() + 1_000_000,
                || true,
            )
            .unwrap();
        let finished = Rc::new(Cell::new(false));
        let done = finished.clone();
        let until = Instant::now() + Duration::from_secs(3);
        let mut nonce = 12_000_u128;
        let driver = async {
            while !finished.get() {
                assert!(Instant::now() < until, "redraw failed to finish");
                host.drive(
                    Duration::from_millis(1),
                    || {
                        nonce += 1;
                        Ok(nonce)
                    },
                    |_, _| Ok(Disposition::Consumed),
                )
                .await
                .unwrap();
            }
        };
        let redraw = async {
            let outcome = stream
                .apply_cursor(&c, &mut |_| {}, &mut |_, _| Ok(Disposition::Blocked))
                .await;
            done.set(true);
            outcome
        };
        let ((), outcome) = Box::pin(support::both(driver, redraw)).await;
        outcome.unwrap();
        let after = stream.statistics();
        assert!(
            after.network_turns > before.network_turns,
            "native redraw starved network service"
        );
        assert_eq!(after.decoded, before.decoded);
        assert_eq!(after.compositor_submissions, before.compositor_submissions);
        assert_eq!(stream.worker_id(), worker);
        assert!(!stream.control.is_stopped());
        let cursor = stream.cursor.as_mut().unwrap();
        assert_eq!(
            cursor.remote.confirmed(),
            Confirmed::Visible {
                shape: 5,
                x: 90,
                y: 30,
            }
        );
        // Completion acknowledged only x=20; x=90 remains pending. There is no
        // chain of redraws ahead of the next pixel-decode admission.
        assert_eq!(
            cursor
                .remote
                .advance(false, None, None, super::super::now(&c).unwrap()),
            Step::Overlay(Some(At {
                shape: 5,
                x: 90,
                y: 30,
            }))
        );
        if lose_picture {
            assert_eq!(
                stream.receiver.failure(),
                Some(DeliveryError::ReferenceExpired)
            );
            assert_eq!(stream.receiver.state(), ReceiveState::NeedsRecovery);
            assert_eq!(
                stream.recovery_state(),
                Some(recovery_control::State::Requested)
            );
            assert_eq!(stream.receiver.budget_usage().pictures, 0);
        } else {
            assert_eq!(stream.receiver.state(), ReceiveState::Streaming);
        }
        // A completion from a retired view cannot populate the new view cache.
        let cursor = stream.cursor.as_mut().unwrap();
        let previous = cursor.lanes.view;
        cursor.lanes.view.recovery = previous.recovery.next().unwrap();
        cursor.complete_overlay(
            previous,
            Some(At {
                shape: 5,
                x: 20,
                y: 30,
            }),
        );
        assert_eq!(cursor.remote.installed, None);
        assert!(matches!(
            cursor.remote.overlay_update(Some(At {
                shape: 5,
                x: 90,
                y: 30,
            })),
            Some(Update::Shape { .. })
        ));
        // Restore the test-only snapshot so teardown still owns the original routes.
        cursor.lanes.view = previous;
        stream
            .reap_media(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
        host.close();
    });
}
