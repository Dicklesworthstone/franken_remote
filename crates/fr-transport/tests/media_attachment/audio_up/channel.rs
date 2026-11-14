//! Real TLS routing and native send custody, with no native microphone claimed.
use super::*;
use fr_core::audio::{AudioChannels, AudioDirection, AudioStopReason};
use fr_transport::quic::audio::{AudioUpChannel, Side};
use fr_wire::audio::{self, AudioConfiguration, AudioPacket, AudioStop};

fn configuration(id: u32, direction: AudioDirection) -> Vec<u8> {
    let cfg = AudioConfiguration {
        direction,
        generation: AudioGeneration::INITIAL,
        channels: AudioChannels::Mono,
        sample_rate: 48_000,
        frame_duration_ms: 20,
        max_packet_bytes: 400,
        max_decoded_samples: 960,
        jitter_target_ms: 40,
    };
    let mut bytes = vec![0; audio::AUDIO_CONFIGURATION_RECORD_BYTES];
    audio::encode_configuration(&cfg, id, &mut bytes).unwrap();
    bytes
}
fn packet(id: u32) -> Vec<u8> {
    let mut bytes = vec![0; audio::AUDIO_PACKET_OVERHEAD + 3];
    let packet = AudioPacket {
        direction: AudioDirection::Uplink,
        generation: AudioGeneration::INITIAL,
        sequence: 0,
        timestamp_samples: 0,
        duration_samples: 960,
        payload: &[1, 2, 3],
    };
    audio::encode_packet(&packet, id, &mut bytes).unwrap();
    bytes
}
fn stop(id: u32) -> Vec<u8> {
    let mut bytes = vec![0; audio::AUDIO_STOP_RECORD_BYTES];
    audio::encode_stop(
        &AudioStop {
            direction: AudioDirection::Uplink,
            generation: AudioGeneration::INITIAL,
            reason: AudioStopReason::UserMute,
        },
        id,
        &mut bytes,
    )
    .unwrap();
    bytes
}

#[test]
fn completed_owner_preserves_selection_and_rejects_equal_id_foreign_connections() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
        let (h, c, _, _) = attach(&mut l, &cx, MediaRole::AudioUp, 9).await;
        let mut host = AudioUpChannel::new(&l.h, h).unwrap();
        let client = AudioUpChannel::new(&l.c, c).unwrap();
        assert_eq!((host.side(), client.side()), (Side::Host, Side::Controller));
        assert_eq!(host.parent(), parent());
        assert_eq!(host.is_retired(&l.h), Ok(false));
        assert_eq!(host.limits(), l.selection.limits);
        assert_eq!((host.binding(), client.channel()), (9, 9));
        assert_eq!(
            (host.maximum_record_bytes(), client.packet_maximum()),
            (1150, 1150)
        );
        let mut foreign = Link::new(&cx).await;
        enable(&mut foreign);
        let _foreign_input = attach(&mut foreign, &cx, MediaRole::Input, 8).await;
        let _foreign_audio = attach(&mut foreign, &cx, MediaRole::AudioUp, 9).await;
        let before = foreign.h.usage();
        assert_eq!(host.check(&foreign.h), Err(Error::WrongRoute));
        assert!(!host.send_expired(&foreign.h));
        assert_eq!(host.is_retired(&foreign.h), Err(Error::WrongRoute));
        assert_eq!(
            host.control_send_drained(&foreign.h),
            Err(Error::WrongRoute)
        );
        assert_eq!(
            host.datagrams_send_drained(&foreign.h),
            Err(Error::WrongRoute)
        );
        assert_eq!(host.retire(&mut foreign.h, &cx), Err(Error::WrongRoute));
        assert_eq!(foreign.h.usage(), before);
        assert!(!foreign.h.is_closed());
        host.check(&l.h).unwrap();
        client.check(&l.c).unwrap();
    });
}

#[test]
fn prepared_admission_checks_direction_and_keeps_real_native_custody() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
        let (h, c, _, _) = attach(&mut l, &cx, MediaRole::AudioUp, 9).await;
        let host = AudioUpChannel::new(&l.h, h).unwrap();
        let client = AudioUpChannel::new(&l.c, c).unwrap();
        quiet(&mut l, &cx).await;
        let until = clock(&cx) + 1_000_000;
        let before = l.c.usage();
        assert_eq!(
            client.send_control_prepared(
                &mut l.c,
                &cx,
                &configuration(9, AudioDirection::Downlink),
                until,
                || true,
                || Ok::<_, ()>(()),
            ),
            Err(Error::WrongRoute)
        );
        assert_eq!(l.c.usage(), before);
        assert_eq!(
            host.send_packet_prepared(
                &mut l.h,
                &cx,
                &packet(9),
                until,
                || true,
                || Ok::<_, ()>(())
            ),
            Err(Error::WrongRoute)
        );
        assert_eq!(
            client.send_packet_prepared(
                &mut l.c,
                &cx,
                &packet(9),
                until,
                || true,
                || Err("expired permit")
            ),
            Ok(SendAdmission::Refused("expired permit"))
        );
        assert!(client.datagrams_send_drained(&l.c).unwrap());
        assert_eq!(
            client.send_packet_prepared(
                &mut l.c,
                &cx,
                &packet(9),
                until,
                || true,
                || Ok::<_, ()>(())
            ),
            Ok(SendAdmission::Accepted)
        );
        assert!(
            !client.datagrams_send_drained(&l.c).unwrap(),
            "queued native audio still needs its permit"
        );
        while !client.datagrams_send_drained(&l.c).unwrap() {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
        }
        let mut got = false;
        while !got {
            assert!(clock(&cx) < until);
            host.dispatch(
                &mut l.h,
                &cx,
                || true,
                |_| true,
                |route, bytes, receipt| {
                    assert!(matches!(route, Route::Datagram(_)));
                    assert_eq!(bytes, packet(9));
                    assert!(receipt.is_some());
                    got = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
            if !got {
                l.drive(&cx).await;
            }
        }
    });
}

#[test]
fn packet_backpressure_preserves_receipt_and_allows_reliable_mute() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
        let (h, c, _, _) = attach(&mut l, &cx, MediaRole::AudioUp, 9).await;
        let host = AudioUpChannel::new(&l.h, h).unwrap();
        let client = AudioUpChannel::new(&l.c, c).unwrap();
        quiet(&mut l, &cx).await;
        let until = clock(&cx) + 1_000_000;
        client
            .send_packet_prepared(
                &mut l.c,
                &cx,
                &packet(9),
                until,
                || true,
                || Ok::<_, ()>(()),
            )
            .unwrap();
        let mut original = None;
        while original.is_none() {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
            host.dispatch(
                &mut l.h,
                &cx,
                || true,
                |_| true,
                |route, bytes, receipt| {
                    assert!(matches!(route, Route::Datagram(_)));
                    assert_eq!(bytes, packet(9));
                    original = receipt;
                    Ok(Disposition::Blocked)
                },
            )
            .unwrap();
        }
        client
            .send_control_prepared(&mut l.c, &cx, &stop(9), until, || true, || Ok::<_, ()>(()))
            .unwrap();
        assert!(!client.control_send_drained(&l.c).unwrap());
        let mut muted = false;
        while !muted {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
            host.dispatch(
                &mut l.h,
                &cx,
                || true,
                |route| matches!(route, Route::Stream(_)),
                |route, bytes, receipt| {
                    assert!(matches!(route, Route::Stream(_)));
                    assert_eq!(bytes, stop(9));
                    assert!(receipt.is_none());
                    muted = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        }
        let mut calls = 0;
        host.dispatch(
            &mut l.h,
            &cx,
            || true,
            |_| true,
            |_, bytes, receipt| {
                assert_eq!(bytes, packet(9));
                assert_eq!(
                    receipt, original,
                    "blocked custody cannot restart its clock"
                );
                calls += 1;
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
        assert_eq!(calls, 1);
        while !client.control_send_drained(&l.c).unwrap() {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
        }
    });
}

#[test]
fn explicit_and_requested_retirement_fence_unwithdrawable_native_microphone_bytes() {
    run_test!(cx, {
        for requested in 0..3 {
            let mut l = Link::new(&cx).await;
            enable(&mut l);
            let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
            let (_h, c, _, _) = attach(&mut l, &cx, MediaRole::AudioUp, 9).await;
            let mut client = AudioUpChannel::new(&l.c, c).unwrap();
            quiet(&mut l, &cx).await;
            client
                .send_packet_prepared(
                    &mut l.c,
                    &cx,
                    &packet(9),
                    clock(&cx) + 1_000_000,
                    || true,
                    || Ok::<_, ()>(()),
                )
                .unwrap();
            assert!(!client.datagrams_send_drained(&l.c).unwrap());
            let result = match requested {
                0 => client.retire(&mut l.c, &cx),
                1 => {
                    client.request_retirement();
                    l.c.tick(&cx, || true)
                }
                _ => {
                    let request = client.retirement_request().clone();
                    request.request();
                    l.c.tick(&cx, || true)
                }
            };
            assert_eq!(result, Err(Error::RetainedMicrophoneOutput));
            assert!(l.c.is_closed());
            assert_eq!(client.is_retired(&l.c), Err(Error::Closed));
            assert!(client.datagrams_send_drained(&l.c).unwrap());
            assert!(client.control_send_drained(&l.c).unwrap());
            assert_eq!(l.c.drive(&cx, WAIT, || true).await, Err(Error::Closed));
            assert!(
                !l.h.is_closed(),
                "the foreign endpoint was not mutated locally"
            );
        }
    });
}

#[test]
fn host_microphone_retirement_preserves_queued_video_and_tombstones_the_lane() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
        let (_vh, _vc, hv, cv) = attach(&mut l, &cx, MediaRole::Video, 9).await;
        let (h, _c, _, _) = attach(&mut l, &cx, MediaRole::AudioUp, 10).await;
        let mut host = AudioUpChannel::new(&l.h, h).unwrap();
        quiet(&mut l, &cx).await;
        let until = clock(&cx) + 1_000_000;
        let video = record(0x34, 9, 300);
        l.h.send(
            &cx,
            Route::Datagram(hv.datagram.unwrap()),
            &video,
            until,
            || true,
        )
        .unwrap();
        assert!(!host.datagrams_send_drained(&l.h).unwrap());
        host.retire(&mut l.h, &cx).unwrap();
        assert!(!l.h.is_closed());
        assert_eq!(host.is_retired(&l.h), Ok(true));
        assert_eq!(host.check(&l.h), Err(Error::Closed));
        assert!(host.control_send_drained(&l.h).unwrap());
        let mut got = false;
        while !got {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
            l.c.receive_ready(
                &cx,
                || true,
                |route| route == Route::Datagram(cv.datagram.unwrap()),
                |_, bytes| {
                    assert_eq!(bytes, video);
                    got = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        }
        assert!(host.datagrams_send_drained(&l.h).unwrap());
        host.retire(&mut l.h, &cx).unwrap();
        l.h.tick(&cx, || true).unwrap();
    });
}

#[test]
fn cloned_retirement_request_preserves_parent_and_cannot_retarget_equal_bindings() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
        let (h, _c, _, _) = attach(&mut l, &cx, MediaRole::AudioUp, 9).await;
        let host = AudioUpChannel::new(&l.h, h).unwrap();
        let request = host.retirement_request();
        let copy = request.clone();
        let mut foreign = Link::new(&cx).await;
        enable(&mut foreign);
        let _foreign_input = attach(&mut foreign, &cx, MediaRole::Input, 8).await;
        let (h, _c, _, _) = attach(&mut foreign, &cx, MediaRole::AudioUp, 9).await;
        let foreign_host = AudioUpChannel::new(&foreign.h, h).unwrap();
        request.request();
        l.h.tick(&cx, || true).unwrap();
        assert_eq!(host.is_retired(&l.h), Ok(true));
        assert!(!l.h.is_closed());
        foreign.h.tick(&cx, || true).unwrap();
        foreign_host.check(&foreign.h).unwrap();
        assert_eq!(foreign_host.is_retired(&foreign.h), Ok(false));
        copy.request();
        l.h.tick(&cx, || true).unwrap();
        assert_eq!(host.is_retired(&l.h), Ok(true));
        assert!(!l.h.is_closed());
    });
}

#[test]
#[allow(clippy::too_many_lines)]
fn microphone_prepared_refusal_preserves_original_send_expiry_at_both_admission_gates() {
    for packet_send in [false, true] {
        for refused_at in [1, 2] {
            run_test!(cx, {
                let mut l = Link::with_policy(
                    &cx,
                    Policy {
                        critical_send_records: 2,
                        ..Policy::default()
                    },
                )
                .await;
                enable(&mut l);
                let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
                let (h, c, _, _) = attach(&mut l, &cx, MediaRole::AudioUp, 9).await;
                let host = AudioUpChannel::new(&l.h, h).unwrap();
                let client = AudioUpChannel::new(&l.c, c).unwrap();
                quiet(&mut l, &cx).await;
                client
                    .send_control_prepared(
                        &mut l.c,
                        &cx,
                        &configuration(9, AudioDirection::Uplink),
                        clock(&cx) + 100_000,
                        || true,
                        || Ok::<_, ()>(()),
                    )
                    .unwrap();
                let mut calls = 0;
                let prepared = || {
                    calls += 1;
                    if calls == refused_at {
                        // Simulate a thread stall during one of the two final
                        // permit checks, while an older record keeps custody.
                        std::thread::sleep(Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US));
                        Err(7_u8)
                    } else {
                        Ok(())
                    }
                };
                let until = clock(&cx) + 4_000_000;
                let result = if packet_send {
                    client.send_packet_prepared(&mut l.c, &cx, &packet(9), until, || true, prepared)
                } else {
                    client.send_control_prepared(&mut l.c, &cx, &stop(9), until, || true, prepared)
                };
                assert_eq!(result, Err(Error::SendExpired));
                assert_eq!(calls, refused_at);
                assert!(client.send_expired(&l.c));
                assert!(!l.c.is_closed());
                assert!(client.control_send_drained(&l.c).unwrap());
                assert!(client.datagrams_send_drained(&l.c).unwrap());
                let until = clock(&cx) + 1_000_000;
                while !host.send_expired(&l.h) {
                    assert!(clock(&cx) < until);
                    l.drive(&cx).await;
                }
                assert_eq!(
                    host.dispatch(
                        &mut l.h,
                        &cx,
                        || true,
                        |_| true,
                        |_, _, _| panic!("an expired or refused audio record escaped"),
                    ),
                    Err(Error::SendExpired)
                );
            });
        }
    }
}

#[test]
fn microphone_admission_expiry_does_not_retire_the_live_audio_pair() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
        let (_h, c, _, _) = attach(&mut l, &cx, MediaRole::AudioUp, 9).await;
        let client = AudioUpChannel::new(&l.c, c).unwrap();
        quiet(&mut l, &cx).await;
        assert_eq!(
            client.send_control_prepared(
                &mut l.c,
                &cx,
                &configuration(9, AudioDirection::Uplink),
                clock(&cx),
                || true,
                || Ok::<_, ()>(()),
            ),
            Err(Error::Expired)
        );
        assert_eq!(
            client.send_packet_prepared(
                &mut l.c,
                &cx,
                &packet(9),
                clock(&cx),
                || true,
                || Ok::<_, ()>(()),
            ),
            Err(Error::Expired)
        );
        assert!(!client.send_expired(&l.c));
        assert_eq!(client.is_retired(&l.c), Ok(false));
        client.check(&l.c).unwrap();
        assert_eq!(l.c.usage().retained_send_records, 0);
        assert!(client.datagrams_send_drained(&l.c).unwrap());
    });
}
