//! Real UDP/TLS through the running controller, original counted input agent,
//! and existing microphone coordinator. Only the native success case needs the
//! real installed workers and a private `PipeWire` daemon; no fake audio backend.
use super::*;
use crate::{microphone_process as native, microphone_quic as uplink};
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStopReason};
use fr_transport::quic::{SendAdmission, audio::AudioUpChannel};
use fr_wire::audio::{self, AudioConfiguration, AudioStop};
use std::path::Path;

async fn selected_fixture(c: &Cx, h: &Cx) -> Fixture {
    let mut capabilities = extended_capabilities(false, false);
    capabilities.push(WireCapability {
        name: fr_wire::audio_up::CAPABILITY.into(),
        version: fr_wire::audio_up::VERSION,
        required: true,
    });
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    Box::pin(fixture_selected(c, h, None, false, capabilities)).await
}
fn absent_launch() -> native::Launch {
    native::Launch::new(
        Path::new("/no-installed-microphone-device"),
        Path::new("/no-installed-microphone-codec"),
        Path::new("/no-installed-microphone-server/pipewire-0"),
    )
    .unwrap()
}
fn request(host: &ControlledHost, timeout: Duration) -> ChannelRequest {
    ChannelRequest {
        binding: Binding {
            parent: ControlBinding {
                id: 14,
                ..host.session.binding()
            },
            display: 9,
            geometry: DisplayGeometryGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
        },
        ticket: Ticket(414),
        timeout,
    }
}
#[allow(clippy::unnecessary_wraps)]
fn only_application(route: Route, bytes: &[u8]) -> Result<Disposition, ()> {
    assert!(!matches!(route, Route::Stream(r) if r.binding == 14));
    assert!(!matches!(bytes.get(6..8), Some([0, 0x18..=0x1c])));
    Ok(Disposition::Blocked)
}
async fn step(host: &mut ControlledHost, viewer: &mut View, c: &Cx, n: &mut u128, t: &mut u128) {
    let (result, ()) = Box::pin(support::both(
        host.drive(
            Duration::from_millis(2),
            || nonce(n),
            || ticket(t),
            only_application,
        ),
        viewer.drive(c),
    ))
    .await;
    result.unwrap();
}
async fn negotiate(
    host: &mut ControlledHost,
    viewer: &mut View,
    c: &Cx,
    n: &mut u128,
    t: &mut u128,
) -> AudioUpChannel {
    let until = now(c).unwrap() + 1_000_000;
    let mut peer: Option<MediaChannel> = None;
    loop {
        assert!(
            now(c).unwrap() < until,
            "ordinary turns must finish AudioUp"
        );
        if let Some(peer) = &mut peer {
            peer.transmit(viewer.session.io().unwrap().0, c, || true)
                .unwrap();
        }
        step(host, viewer, c, n, t).await;
        if peer.is_none() {
            let mut offer = None;
            viewer
                .session
                .io()
                .unwrap()
                .0
                .receive(
                    c,
                    || true,
                    |route, bytes| {
                        if matches!(route, Route::Stream(r) if r.binding == 7)
                            && bytes.get(6..8) == Some(&(Kind::StreamBinding as u16).to_be_bytes())
                        {
                            offer = Some(bytes.to_vec());
                            Ok(Disposition::Consumed)
                        } else {
                            Ok(Disposition::Blocked)
                        }
                    },
                )
                .unwrap();
            if let Some(bytes) = offer {
                peer = Some(
                    viewer
                        .session
                        .accept_media_channel(&bytes, Duration::from_secs(1))
                        .unwrap(),
                );
            }
        }
        if let Some(channel) = &mut peer {
            let q = viewer.session.io().unwrap().0;
            channel.dispatch(q, c, || true).unwrap();
            let complete = channel.finish(q, c, || true).unwrap().is_some();
            if complete && !host.microphone_negotiating() {
                return AudioUpChannel::new(q, peer.take().unwrap()).unwrap();
            }
        }
    }
}
fn configuration() -> Vec<u8> {
    let mut bytes = vec![0; audio::AUDIO_CONFIGURATION_RECORD_BYTES];
    audio::encode_configuration(
        &AudioConfiguration {
            direction: AudioDirection::Uplink,
            generation: AudioGeneration::from_raw(1),
            channels: AudioChannels::Mono,
            sample_rate: 48_000,
            frame_duration_ms: 10,
            max_packet_bytes: 1000,
            max_decoded_samples: 480,
            jitter_target_ms: 20,
        },
        14,
        &mut bytes,
    )
    .unwrap();
    bytes
}
fn send(peer: &AudioUpChannel, viewer: &mut View, c: &Cx, bytes: &[u8]) {
    assert_eq!(
        peer.send_control_prepared(
            viewer.session.io().unwrap().0,
            c,
            bytes,
            now(c).unwrap() + 1_000_000,
            || true,
            || Ok::<_, ()>(()),
        )
        .unwrap(),
        SendAdmission::Accepted
    );
}

#[test]
fn microphone_consent_and_selection_refuse_before_reserving_or_starting_work() {
    // Session close cancels its original contexts. Each refusal must establish
    // its own owner region instead of reusing the previous closed connection's.
    for selected in [false, true] {
        run(|c, h| async move {
            let Fixture {
                mut host,
                viewer: _viewer,
                driver,
                seat,
                ..
            } = if selected {
                Box::pin(selected_fixture(&c, &h)).await
            } else {
                Box::pin(fixture(&c, &h, None)).await
            };
            let before = host.next_channel_binding().unwrap();
            let offer = request(&host, Duration::from_secs(1));
            let reason = host
                .offer_microphone(offer, !selected, absent_launch())
                .unwrap_err();
            assert_eq!(
                reason,
                if selected {
                    uplink::Error::Permission
                } else {
                    uplink::Error::NotSelected
                }
            );
            assert_eq!(host.next_channel_binding().unwrap(), before);
            assert!(!host.microphone_negotiating());
            assert!(host.microphone_retirement().is_none());
            assert!(host.io().is_ok());
            host.close();
            assert!(driver.await.handoff_safe());
            assert!(!seat.is_occupied());
        });
    }
}

#[test]
fn ordinary_control_turns_join_microphone_and_isolate_native_start_failure() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            mut viewer,
            driver,
            effects,
            seat,
            ..
        } = Box::pin(selected_fixture(&c, &h)).await;
        let (mut n, mut t) = (20_000, 50_000);
        let ((), shutdown) = Box::pin(support::both(
            async {
                host.offer_microphone(
                    request(&host, Duration::from_secs(1)),
                    true,
                    absent_launch(),
                )
                .unwrap();
                assert!(matches!(host.io(), Err(Error::Order)));
                assert_eq!(host.next_channel_binding().unwrap(), 15);
                let peer = negotiate(&mut host, &mut viewer, &c, &mut n, &mut t).await;
                assert_eq!(host.microphone_state(), Some(uplink::State::Idle));
                assert!(host.microphone_retirement().is_none());
                assert!(matches!(host.io(), Err(Error::Order)));
                send(&peer, &mut viewer, &c, &configuration());
                let until = now(&c).unwrap() + 1_000_000;
                while host.microphone_state() != Some(uplink::State::Retired) {
                    assert!(now(&c).unwrap() < until, "native failure must be bounded");
                    step(&mut host, &mut viewer, &c, &mut n, &mut t).await;
                }
                assert_eq!(
                    host.microphone_reason(),
                    Some(uplink::Error::Native(native::Error::Spawn))
                );
                assert_eq!(
                    host.microphone_statistics()
                        .unwrap()
                        .acknowledgements_admitted,
                    0
                );
                assert!(!host.control().is_stopped());
                assert!(host.io().is_ok());
                assert_eq!(
                    host.offer_microphone(
                        request(&host, Duration::from_secs(1)),
                        true,
                        absent_launch()
                    )
                    .unwrap_err(),
                    uplink::Error::AlreadyAttached
                );
                viewer.visible(&c);
                viewer.queue_key(&c, true);
                let until = now(&c).unwrap() + 1_000_000;
                while viewer.receipts == 0 {
                    assert!(now(&c).unwrap() < until, "microphone failure held input");
                    step(&mut host, &mut viewer, &c, &mut n, &mut t).await;
                }
                assert_eq!(effects.lock().unwrap().events, vec![true]);
                host.retire_microphone().unwrap();
                host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
        assert_eq!(effects.lock().unwrap().events, vec![true, false]);
    });
}

#[test]
fn unfinished_microphone_timeout_is_fixed_and_never_starts_native_work() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            viewer: _viewer,
            driver,
            seat,
            ..
        } = Box::pin(selected_fixture(&c, &h)).await;
        host.offer_microphone(
            request(&host, Duration::from_millis(10)),
            true,
            absent_launch(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(15));
        let result = host
            .drive(Duration::ZERO, || Ok(2000), || None, only_application)
            .await;
        assert!(result.is_err());
        assert_eq!(host.microphone_reason(), Some(uplink::Error::SetupExpired));
        assert!(host.microphone_retirement().is_none());
        assert!(host.control().is_stopped());
        assert!(host.io().is_err());
        assert!(driver.await.handoff_safe());
        assert!(!seat.is_occupied());
    });
}

#[test]
fn abandoning_an_unpolled_controller_drive_fences_its_pending_microphone() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            viewer: _viewer,
            driver,
            seat,
            ..
        } = Box::pin(selected_fixture(&c, &h)).await;
        host.offer_microphone(
            request(&host, Duration::from_secs(1)),
            true,
            absent_launch(),
        )
        .unwrap();
        drop(host.drive(Duration::ZERO, || Ok(2000), || None, only_application));
        assert!(!host.microphone_negotiating());
        assert_eq!(host.microphone_reason(), Some(uplink::Error::Cancelled));
        assert!(host.microphone_retirement().is_none());
        assert!(host.control().is_stopped());
        assert!(host.io().is_err());
        assert!(driver.await.handoff_safe());
        assert!(!seat.is_occupied());
    });
}

#[test]
#[ignore = "requires actual device/Opus worker images and a private PipeWire daemon"]
fn normal_controller_turns_commit_native_microphone_then_mute_and_reap_both_children() {
    let server = crate::microphone_quic::tests::device::Server::start();
    run(|c, h| async move {
        let Fixture {
            mut host,
            mut viewer,
            driver,
            seat,
            ..
        } = Box::pin(selected_fixture(&c, &h)).await;
        let (mut n, mut t) = (20_000, 50_000);
        let ((), shutdown) = Box::pin(support::both(
            async {
                host.offer_microphone(
                    request(&host, Duration::from_secs(1)),
                    true,
                    server.launch(),
                )
                .unwrap();
                let peer = negotiate(&mut host, &mut viewer, &c, &mut n, &mut t).await;
                send(&peer, &mut viewer, &c, &configuration());
                let until = now(&c).unwrap() + 1_500_000;
                let mut acknowledged = false;
                while host.microphone_state() != Some(uplink::State::Running) || !acknowledged {
                    assert!(
                        now(&c).unwrap() < until,
                        "real readiness/ACK/private Commit: {:?}",
                        host.microphone_reason()
                    );
                    step(&mut host, &mut viewer, &c, &mut n, &mut t).await;
                    peer.dispatch(
                        viewer.session.io().unwrap().0,
                        &c,
                        || true,
                        |_| true,
                        |_, bytes, receipt| {
                            assert!(receipt.is_none());
                            let ack = audio::decode_configured(bytes, 14).unwrap();
                            assert!(ack.accepted);
                            assert_eq!(ack.direction, AudioDirection::Uplink);
                            assert_eq!(ack.generation, AudioGeneration::from_raw(1));
                            acknowledged = true;
                            Ok(Disposition::Consumed)
                        },
                    )
                    .unwrap();
                }
                assert_eq!(
                    host.microphone_statistics()
                        .unwrap()
                        .acknowledgements_admitted,
                    1
                );
                let retirement = host.microphone_retirement().unwrap();
                let mut stop = vec![0; audio::AUDIO_STOP_RECORD_BYTES];
                audio::encode_stop(
                    &AudioStop {
                        direction: AudioDirection::Uplink,
                        generation: AudioGeneration::from_raw(1),
                        reason: AudioStopReason::UserMute,
                    },
                    14,
                    &mut stop,
                )
                .unwrap();
                send(&peer, &mut viewer, &c, &stop);
                let until = now(&c).unwrap() + 1_000_000;
                while !retirement.is_complete()
                    || host.microphone_state() != Some(uplink::State::Idle)
                {
                    assert!(now(&c).unwrap() < until, "actual child exits are required");
                    step(&mut host, &mut viewer, &c, &mut n, &mut t).await;
                }
                let receipts = retirement.receipts();
                assert_eq!(receipts.device, native::ChildExit::Reaped);
                assert_eq!(receipts.codec, native::ChildExit::Reaped);
                assert!(!host.control().is_stopped());
                host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
    });
}
