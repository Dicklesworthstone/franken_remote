#![cfg(all(target_os = "linux", feature = "linux-microphone-uplink"))]
//! Real certificate-authenticated UDP/TLS, original accepted controller and
//! completed input/AudioUp attachments. Authority/presentation are explicitly
//! local policy fixtures. Ignored native cases additionally use a real named
//! PulseAudio recording source, encoder child and independent Opus decoder.
//! This does not qualify a host virtual microphone, physical OS consent or UI.
#[allow(dead_code)]
#[path = "../../fr-client/tests/clipboard_controller/mod.rs"]
mod authority;
#[allow(dead_code)]
#[path = "../../fr-transport/tests/support/mod.rs"]
mod network;
#[allow(dead_code)]
mod pulse_support;
#[path = "microphone_network/support.rs"]
mod support;

use asupersync::cx::Cx;
use fr_core::audio::{
    AudioDirection, AudioGeneration, AudioStreamConfig, MicPermission, MicTalkMode,
};
use fr_media::audio::{AudioAccessUnit, AudioDecoder};
use fr_native::{
    audio_process::{
        Retirement,
        network::{Error, State, Uplink},
    },
    opus::Decoder,
};
use fr_transport::quic::{Disposition, Route};
use fr_wire::audio::{self, AudioConfigured};
use pulse_support::{
    Server,
    microphone::{Feeder, assert_recording_stopped, power},
};
use std::{
    path::Path,
    time::{Duration, Instant},
};
use support::{CHANNEL, Link, WAIT};

macro_rules! run_test {
    ($cx:ident, $body:expr) => {{
        network::runtime().block_on(async {
            let $cx = Cx::current().unwrap();
            $body
        });
    }};
}

#[test]
fn constructor_permission_and_selection_refusals_retire_only_the_proven_optional_lane() {
    for changed_selection in [false, true] {
        run_test!(cx, {
            let mut link = Link::new(&cx).await;
            let (_host, client) = link.microphone(&cx).await;
            let (mut input, origin) = link.controller(&cx).await;
            let mut selected = link.selection.clone();
            if changed_selection {
                selected.limits = fr_core::limits::ProtocolLimits::with_overrides(
                    fr_core::limits::LimitOverrides {
                        max_control_message_bytes: Some(2048),
                        ..fr_core::limits::LimitOverrides::default()
                    },
                )
                .unwrap();
            }
            let refused = Uplink::attach(
                &mut link.client,
                client,
                &mut input,
                &selected,
                changed_selection,
                support::options(Path::new("/nonexistent/private-pulse")),
                &cx,
                || support::now(origin),
            )
            .unwrap_err();
            assert_eq!(
                refused.reason(),
                if changed_selection {
                    Error::WrongScope
                } else {
                    Error::Consent(fr_client::audio::uplink::Error::Permission)
                }
            );
            drop(refused);
            link.client.tick(&cx, || true).unwrap();
            input.tick(support::now(origin)).unwrap();
            link.assert_original_control_usable(&cx).await;
        });
    }
}

#[test]
fn constructor_returns_foreign_connection_proof_without_abandoning_either_owner() {
    run_test!(cx, {
        let mut link = Link::new(&cx).await;
        let (_host, client) = link.microphone(&cx).await;
        let (mut input, origin) = link.controller(&cx).await;
        let mut foreign = Link::new(&cx).await;
        let _foreign_microphone = foreign.microphone(&cx).await;
        let refused = Uplink::attach(
            &mut foreign.client,
            client,
            &mut input,
            &link.selection,
            true,
            support::options(Path::new("/nonexistent/private-pulse")),
            &cx,
            || panic!("foreign connection must not sample/mutate original input owner"),
        )
        .unwrap_err();
        let (_, client) = refused.into_parts();
        client.check(&link.client).unwrap();
        link.client.tick(&cx, || true).unwrap();
        foreign.client.tick(&cx, || true).unwrap();
        let mut uplink = link.uplink(
            &cx,
            client,
            &mut input,
            origin,
            Path::new("/nonexistent/private-pulse"),
        );
        assert_eq!(uplink.state(), State::Muted);
        uplink.retire(&mut link.client).unwrap();
        link.assert_original_control_usable(&cx).await;
        foreign.assert_original_control_usable(&cx).await;
    });
}

#[test]
fn exact_host_ack_and_explicit_local_permission_precede_any_capture_resource() {
    run_test!(cx, {
        let mut link = Link::new(&cx).await;
        let (host, client) = link.microphone(&cx).await;
        let (mut input, origin) = link.controller(&cx).await;
        let mut uplink = link.uplink(
            &cx,
            client,
            &mut input,
            origin,
            Path::new("/nonexistent/private-pulse"),
        );
        assert_eq!(uplink.state(), State::Muted);
        assert_eq!(uplink.permission(), MicPermission::NotRequested);
        assert!(
            uplink
                .begin_talk(MicTalkMode::OpenMic { active: true })
                .is_err()
        );
        uplink.set_permission(MicPermission::Granted).unwrap();
        for _ in 0..3 {
            link.drive_uplink(&cx, &mut uplink).await;
        }
        assert!(
            uplink.retirement().is_none(),
            "permission alone never starts recording"
        );
        let offer = uplink
            .begin_talk(MicTalkMode::OpenMic { active: true })
            .unwrap();
        let bytes = link.next_record(&cx, &mut uplink, &host).await;
        assert_eq!(audio::decode_configuration(&bytes, CHANNEL).unwrap(), offer);
        for _ in 0..3 {
            link.drive_uplink(&cx, &mut uplink).await;
        }
        assert_eq!(uplink.state(), State::AwaitingHost);
        assert!(
            uplink.retirement().is_none(),
            "configuration admission is not an ACK"
        );
        link.acknowledge(&cx, &host, offer, false);
        let until = network::clock(&cx) + 500_000;
        while uplink.state() != State::Stopped {
            assert!(network::clock(&cx) < until);
            link.drive_uplink(&cx, &mut uplink).await;
        }
        assert!(
            uplink.retirement().is_none(),
            "host refusal opened a source"
        );
        link.assert_original_control_usable(&cx).await;
    });
}

#[test]
fn late_positive_ack_after_mute_never_revives_cancelled_talk() {
    run_test!(cx, {
        let mut link = Link::new(&cx).await;
        let (host, client) = link.microphone(&cx).await;
        let (mut input, origin) = link.controller(&cx).await;
        let mut uplink = link.uplink(
            &cx,
            client,
            &mut input,
            origin,
            Path::new("/nonexistent/private-pulse"),
        );
        uplink.set_permission(MicPermission::Granted).unwrap();
        let first = uplink
            .begin_talk(MicTalkMode::OpenMic { active: true })
            .unwrap();
        let bytes = link.next_record(&cx, &mut uplink, &host).await;
        assert_eq!(audio::decode_configuration(&bytes, CHANNEL).unwrap(), first);
        // The remote transport has completely ACKed the original config before
        // this delayed application reply. Only then can mute preserve this lane
        // for another explicit generation without retaining old config bytes.
        let drained_by = network::clock(&cx) + 500_000;
        while link.client.usage().retained_send_records != 0 {
            assert!(
                network::clock(&cx) < drained_by,
                "original configuration did not drain"
            );
            link.drive_uplink(&cx, &mut uplink).await;
        }
        link.acknowledge(&cx, &host, first, true);
        uplink.mute().unwrap();
        let bytes = link.next_record(&cx, &mut uplink, &host).await;
        assert_eq!(
            audio::decode_stop(&bytes, CHANNEL).unwrap().generation,
            first.generation
        );
        assert_eq!(uplink.state(), State::Muted);
        assert!(
            uplink.retirement().is_none(),
            "late ACK must not open even an absent source"
        );
        let until = network::clock(&cx) + 500_000;
        let next = loop {
            match uplink.begin_talk(MicTalkMode::OpenMic { active: true }) {
                Ok(next) => break next,
                Err(Error::Busy) => {
                    assert!(network::clock(&cx) < until);
                    link.drive_uplink(&cx, &mut uplink).await;
                }
                other => panic!("fresh explicit talk refused: {other:?}"),
            }
        };
        assert!(next.generation > first.generation);
        assert!(uplink.retirement().is_none());
        let bytes = link.next_record(&cx, &mut uplink, &host).await;
        assert_eq!(audio::decode_configuration(&bytes, CHANNEL).unwrap(), next);
        // The first ACK remains unable to start the second generation.
        link.acknowledge(&cx, &host, first, true);
        for _ in 0..4 {
            link.drive_uplink(&cx, &mut uplink).await;
        }
        assert_eq!(uplink.state(), State::AwaitingHost);
        assert!(uplink.retirement().is_none());
        uplink.retire(&mut link.client).unwrap();
        link.assert_original_control_usable(&cx).await;
    });
}

#[test]
fn original_owner_talk_and_os_withdrawal_fence_config_after_admission_before_io() {
    for withdrawal in 0..3 {
        run_test!(cx, {
            let mut link = Link::new(&cx).await;
            let (_host, client) = link.microphone(&cx).await;
            let (input, origin) = link.controller(&cx).await;
            let mut input = Some(input);
            let mut uplink = link.uplink(
                &cx,
                client,
                input.as_mut().unwrap(),
                origin,
                Path::new("/nonexistent/private-pulse"),
            );
            uplink.set_permission(MicPermission::Granted).unwrap();
            uplink
                .begin_talk(MicTalkMode::OpenMic { active: true })
                .unwrap();
            uplink.service(&mut link.client, || true).unwrap();
            assert!(
                link.client.usage().retained_send_records > 0,
                "configuration is actually queued"
            );
            match withdrawal {
                0 => {
                    drop(input.take());
                }
                1 => uplink.mute().unwrap(),
                _ => uplink.set_permission(MicPermission::Denied).unwrap(),
            }
            // The parent gate intentionally remains TRUE. The retained original
            // configuration witness must independently retire before first flush.
            let (host, client) = network::both(
                link.host.drive(&cx, WAIT, || true),
                link.client.drive(&cx, WAIT, || uplink.permits_io()),
            )
            .await;
            host.unwrap();
            client.unwrap();
            uplink.service(&mut link.client, || true).unwrap();
            assert_eq!(uplink.state(), State::Stopped);
            assert!(uplink.retirement().is_none());
            assert_eq!(link.client.usage().retained_send_records, 0);
            let mut configuration_delivered = false;
            link.host
                .receive(
                    &cx,
                    || true,
                    |_, bytes| {
                        configuration_delivered |= audio::record_kind(bytes) == Some(0x60);
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            assert!(
                !configuration_delivered,
                "revoked original configuration escaped custody"
            );
            link.assert_original_control_usable(&cx).await;
        });
    }
}

#[test]
fn owner_loss_inside_dispatch_authorization_retires_only_the_actual_optional_lane() {
    run_test!(cx, {
        let mut link = Link::new(&cx).await;
        let (_host, client) = link.microphone(&cx).await;
        let (input, origin) = link.controller(&cx).await;
        let mut input = Some(input);
        let mut uplink = link.uplink(
            &cx,
            client,
            input.as_mut().unwrap(),
            origin,
            Path::new("/nonexistent/private-pulse"),
        );
        uplink.set_permission(MicPermission::Granted).unwrap();
        uplink
            .begin_talk(MicTalkMode::OpenMic { active: true })
            .unwrap();
        uplink.service(&mut link.client, || true).unwrap();
        assert!(link.client.usage().retained_send_records > 0);
        let mut calls = 0;
        let progress = uplink
            .service(&mut link.client, || {
                calls += 1;
                drop(input.take());
                true
            })
            .unwrap();
        assert!(
            calls > 1,
            "original config closes during the bounded dispatch turn"
        );
        assert_eq!(progress.state, State::Stopped);
        assert!(uplink.retirement().is_none());
        assert!(
            !link.client.is_closed(),
            "proved optional retirement must not kill input/video"
        );
        link.assert_original_control_usable(&cx).await;
    });
}

#[test]
fn a_panicking_session_authorization_callback_cannot_leave_capture_or_connection_reusable() {
    run_test!(cx, {
        let mut link = Link::new(&cx).await;
        let (_host, client) = link.microphone(&cx).await;
        let (mut input, origin) = link.controller(&cx).await;
        let mut uplink = link.uplink(
            &cx,
            client,
            &mut input,
            origin,
            Path::new("/nonexistent/private-pulse"),
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            uplink.service(&mut link.client, || {
                panic!("fixture session authorization panic")
            })
        }));
        assert!(result.is_err());
        assert!(link.client.is_closed());
        assert_eq!(uplink.state(), State::Stopped);
        assert!(uplink.retirement().is_none());
    });
}

#[test]
fn foreign_equal_id_connection_cannot_service_retire_or_cancel_the_original_owner() {
    run_test!(cx, {
        let mut link = Link::new(&cx).await;
        let (_host, client) = link.microphone(&cx).await;
        let (mut input, origin) = link.controller(&cx).await;
        let mut uplink = link.uplink(
            &cx,
            client,
            &mut input,
            origin,
            Path::new("/nonexistent/private-pulse"),
        );
        let mut foreign = Link::new(&cx).await;
        let _foreign_audio = foreign.microphone(&cx).await;
        assert_eq!(
            uplink.service(&mut foreign.client, || true),
            Err(Error::WrongConnection)
        );
        assert_eq!(
            uplink.retire(&mut foreign.client),
            Err(Error::WrongConnection)
        );
        drop(uplink.drive(&mut foreign.client, WAIT, || true));
        assert_eq!(uplink.state(), State::Muted);
        assert!(!link.client.is_closed() && !foreign.client.is_closed());
        uplink.retire(&mut link.client).unwrap();
        link.assert_original_control_usable(&cx).await;
        foreign.assert_original_control_usable(&cx).await;
    });
}

#[test]
fn unpolled_original_drive_cancellation_fences_capture_and_connection() {
    run_test!(cx, {
        let mut link = Link::new(&cx).await;
        let (_host, client) = link.microphone(&cx).await;
        let (mut input, origin) = link.controller(&cx).await;
        let mut uplink = link.uplink(
            &cx,
            client,
            &mut input,
            origin,
            Path::new("/nonexistent/private-pulse"),
        );
        uplink.set_permission(MicPermission::Granted).unwrap();
        uplink
            .begin_talk(MicTalkMode::OpenMic { active: true })
            .unwrap();
        drop(uplink.drive(&mut link.client, WAIT, || true));
        assert!(link.client.is_closed());
        assert_eq!(uplink.state(), State::Stopped);
        assert!(uplink.retirement().is_none());
    });
}

#[test]
fn loss_of_the_original_input_owner_refuses_a_later_positive_ack() {
    run_test!(cx, {
        let mut link = Link::new(&cx).await;
        let (host, client) = link.microphone(&cx).await;
        let (mut input, origin) = link.controller(&cx).await;
        let mut uplink = link.uplink(
            &cx,
            client,
            &mut input,
            origin,
            Path::new("/nonexistent/private-pulse"),
        );
        uplink.set_permission(MicPermission::Granted).unwrap();
        let offer = uplink
            .begin_talk(MicTalkMode::OpenMic { active: true })
            .unwrap();
        link.next_record(&cx, &mut uplink, &host).await;
        drop(input);
        link.acknowledge(&cx, &host, offer, true);
        let until = network::clock(&cx) + 500_000;
        while uplink.state() != State::Stopped {
            assert!(network::clock(&cx) < until);
            link.drive_uplink(&cx, &mut uplink).await;
        }
        assert!(uplink.retirement().is_none());
        assert!(
            uplink
                .begin_talk(MicTalkMode::OpenMic { active: true })
                .is_err()
        );
        link.assert_original_control_usable(&cx).await;
    });
}

#[test]
fn a_wrong_generation_or_direction_ack_retires_only_the_optional_lane() {
    for wrong_direction in [false, true] {
        run_test!(cx, {
            let mut link = Link::new(&cx).await;
            let (host, client) = link.microphone(&cx).await;
            let (mut input, origin) = link.controller(&cx).await;
            let mut uplink = link.uplink(
                &cx,
                client,
                &mut input,
                origin,
                Path::new("/nonexistent/private-pulse"),
            );
            uplink.set_permission(MicPermission::Granted).unwrap();
            let offer = uplink
                .begin_talk(MicTalkMode::OpenMic { active: true })
                .unwrap();
            link.next_record(&cx, &mut uplink, &host).await;
            let mut bytes = [0; audio::AUDIO_CONFIGURED_RECORD_BYTES];
            audio::encode_configured(
                &AudioConfigured {
                    direction: if wrong_direction {
                        AudioDirection::Downlink
                    } else {
                        AudioDirection::Uplink
                    },
                    generation: if wrong_direction {
                        offer.generation
                    } else {
                        AudioGeneration::from_raw(offer.generation.as_raw() + 1)
                    },
                    accepted: true,
                    actual_channels: offer.channels,
                    actual_sample_rate: offer.sample_rate,
                    actual_frame_duration_ms: offer.frame_duration_ms,
                },
                CHANNEL,
                &mut bytes,
            )
            .unwrap();
            // A malicious peer can bypass its semantic convenience wrapper;
            // the receiving coordinator must still reject the owned record.
            link.host
                .send(
                    &cx,
                    Route::Stream(link.microphone_out.unwrap()),
                    &bytes,
                    network::clock(&cx) + 1_000_000,
                    || true,
                )
                .unwrap();
            let until = network::clock(&cx) + 500_000;
            while uplink.state() != State::Stopped {
                assert!(network::clock(&cx) < until);
                link.drive_uplink(&cx, &mut uplink).await;
            }
            assert!(uplink.retirement().is_none());
            link.assert_original_control_usable(&cx).await;
        });
    }
}

fn reaped(retirement: &Retirement) {
    let until = Instant::now() + Duration::from_secs(3);
    while !retirement.is_complete() {
        assert!(
            Instant::now() < until,
            "actual capture child was not reaped"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(retirement.process_id(), None);
}
fn has_tone(decoder: &mut Decoder, packet: audio::AudioPacket<'_>) -> bool {
    decoder
        .submit_packet(
            &AudioAccessUnit::new(
                packet.direction,
                packet.generation,
                packet.sequence,
                packet.timestamp_samples,
                packet.duration_samples,
                false,
                packet.payload,
            )
            .unwrap(),
        )
        .unwrap();
    let pcm = decoder.poll_pcm().unwrap().unwrap();
    let tone = power(pcm.samples(), 1000.0);
    tone > 1.0e11
        && [600.0, 800.0, 1200.0, 1500.0]
            .into_iter()
            .all(|hz| tone > 30.0 * power(pcm.samples(), hz))
}

#[test]
#[ignore = "actual UDP/TLS, private PulseAudio source, isolated encoder and independent Opus decode; explicit native CI"]
fn real_capture_traverses_the_original_authenticated_uplink_and_drained_mute_keeps_control() {
    let server = Server::start();
    let _feeder = Feeder::start(&server);
    run_test!(cx, {
        let mut link = Link::new(&cx).await;
        let (host, client) = link.microphone(&cx).await;
        let (mut input, origin) = link.controller(&cx).await;
        let mut uplink = link.uplink(&cx, client, &mut input, origin, &server.socket);
        uplink.set_permission(MicPermission::Granted).unwrap();
        let offer = uplink
            .begin_talk(MicTalkMode::OpenMic { active: true })
            .unwrap();
        link.next_record(&cx, &mut uplink, &host).await;
        assert_recording_stopped(&server);
        link.acknowledge(&cx, &host, offer, true);
        let mut decoder = Decoder::new();
        decoder
            .configure(
                AudioStreamConfig::new(
                    AudioDirection::Uplink,
                    offer.generation,
                    offer.channels,
                    offer.frame_duration_ms,
                    offer.jitter_target_ms,
                )
                .unwrap(),
            )
            .unwrap();
        let (mut packets, mut tones, mut next) = (0_usize, 0_usize, 0_u64);
        let until = network::clock(&cx) + 700_000;
        while network::clock(&cx) < until {
            link.drive_uplink(&cx, &mut uplink).await;
            assert_ne!(
                uplink.state(),
                State::Stopped,
                "native uplink stopped: {:?}",
                uplink.reason()
            );
            host.dispatch(
                &mut link.host,
                &cx,
                || true,
                |_| true,
                |_, bytes, receipt| {
                    let receipt = receipt.expect("audio packet retains original transport receipt");
                    assert!(receipt.remaining_micros(network::clock(&cx)).is_ok());
                    let packet = audio::decode_packet(bytes, CHANNEL).unwrap();
                    assert_eq!(packet.direction, AudioDirection::Uplink);
                    assert_eq!(packet.generation, offer.generation);
                    assert_eq!(packet.sequence, next);
                    assert_eq!(packet.timestamp_samples, next * 960);
                    next += 1;
                    tones += usize::from(has_tone(&mut decoder, packet));
                    packets += 1;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        }
        assert!(packets >= 10, "only {packets} real network audio packets");
        assert!(
            tones * 2 > packets,
            "independent tone decoded in {tones}/{packets}"
        );
        let retirement = uplink.retirement().expect("actual native capture owner");
        // Finish retained datagrams without performing another capture service
        // turn after the native flush, then mute before any new admission.
        let (host_io, client_io) = network::both(
            link.host.drive(&cx, WAIT, || true),
            link.client.drive(&cx, WAIT, || uplink.permits_io()),
        )
        .await;
        host_io.unwrap();
        client_io.unwrap();
        uplink.mute().unwrap();
        uplink.service(&mut link.client, || true).unwrap();
        assert_eq!(uplink.state(), State::Muted);
        reaped(&retirement);
        assert_recording_stopped(&server);
        link.assert_original_control_usable(&cx).await;
        println!(
            "original-controller UDP/TLS microphone: {packets} packets; {tones} decoded tones; artifacts={}",
            server.directory.display()
        );
    });
}

#[test]
#[ignore = "actual UDP/TLS, private PulseAudio source and queued original capture guards; explicit native CI"]
fn muting_with_native_audio_still_queued_fences_before_any_further_network_poll() {
    let server = Server::start();
    let _feeder = Feeder::start(&server);
    run_test!(cx, {
        let mut link = Link::new(&cx).await;
        let (host, client) = link.microphone(&cx).await;
        let (mut input, origin) = link.controller(&cx).await;
        let mut uplink = link.uplink(&cx, client, &mut input, origin, &server.socket);
        uplink.set_permission(MicPermission::Granted).unwrap();
        let offer = uplink
            .begin_talk(MicTalkMode::OpenMic { active: true })
            .unwrap();
        link.next_record(&cx, &mut uplink, &host).await;
        link.acknowledge(&cx, &host, offer, true);
        // Transport only the ACK. After source creation, service capture while
        // intentionally NEVER driving the native client's outbound datagrams.
        let acknowledged_by = network::clock(&cx) + 1_000_000;
        while uplink.retirement().is_none() {
            assert!(
                network::clock(&cx) < acknowledged_by,
                "actual host acknowledgement did not arrive"
            );
            link.drive(&cx).await;
            uplink.service(&mut link.client, || true).unwrap();
            assert_ne!(
                uplink.state(),
                State::Stopped,
                "source refused: {:?}",
                uplink.reason()
            );
        }
        let until = network::clock(&cx) + 700_000;
        loop {
            assert!(
                network::clock(&cx) < until,
                "capture produced no native-queued packet"
            );
            let progress = uplink.service(&mut link.client, || true).unwrap();
            if progress.admitted_packets != 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let retirement = uplink.retirement().unwrap();
        uplink.mute().unwrap();
        assert!(
            !uplink.permits_io(),
            "mute must invalidate retained exact egress"
        );
        assert_eq!(
            uplink.service(&mut link.client, || true),
            Err(Error::EgressExpired)
        );
        assert!(
            link.client.is_closed(),
            "route retirement cannot retract native datagrams"
        );
        reaped(&retirement);
        assert_recording_stopped(&server);
    });
}
