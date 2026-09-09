//! Real Asupersync TLS/UDP integration. No manually established connections,
//! simulated socket, second QUIC stack or runtime. Authority is explicitly
//! synthetic here; these tests do not qualify Tailscale or desktop permissions.
#![cfg(target_os = "linux")]
mod support;
use asupersync::{cx::Cx, types::CancelKind};
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::delivery::{
    DeliveryMode, MediaBindings, MediaBudget, MediaEpoch, ReceiveConfig, ReceivePipeline,
    ReceivePolicy, SendCache, SendPolicy,
};
use fr_transport::quic::*;
use fr_wire::{
    Channel, FrameDescriptor, MediaLimits, PipelineState, Progress, Record, RecoveryChunk,
    SourceObservation, decode_recovery, encode_recovery,
};
use std::{future::Future, pin::pin, task::Poll, time::Duration};
use support::{clock, drive, pair, runtime};

fn recovery_packet(binding: u32, count: usize) -> Vec<u8> {
    let mut out = vec![0; 65536];
    let data = vec![81; count];
    let l = MediaLimits::new(ProtocolLimits::ABSOLUTE, 65536, 16384, 64).unwrap();
    let size = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: u32::try_from(count).unwrap(),
            offset: 0,
            capture_micros: 17,
            bytes: &data,
        },
        binding,
        &l,
        &mut out,
    )
    .unwrap();
    out.truncate(size);
    out
}
#[test]
fn authenticated_stream_reassembles_large_record_and_coalesced_records() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let route = Route::Stream(p.host_routes[1]);
        let big = recovery_packet(2, 32000);
        let small = recovery_packet(2, 600);
        p.server
            .send(&cx, route, &big, clock(&cx) + 2_000_000, || true)
            .unwrap();
        p.server
            .send(&cx, route, &small, clock(&cx) + 2_000_000, || true)
            .unwrap();
        let mut received = vec![];
        for _ in 0..500 {
            drive(&cx, &mut p).await;
            p.client
                .receive(
                    &cx,
                    || true,
                    |r, b| {
                        assert_eq!(
                            r,
                            Route::Stream(StreamRoute {
                                outbound: false,
                                ..p.host_routes[1]
                            })
                        );
                        received.push(b.to_vec());
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if received.len() == 2 {
                break;
            }
        }
        assert_eq!(received, [big, small]);
        assert!(p.client.usage().framed_capacity <= 65536 + 1150);
    });
}
#[test]
fn final_authority_refusal_and_expired_send_never_admit_bytes() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let packet = recovery_packet(2, 100);
        let route = Route::Stream(p.host_routes[1]);
        assert_eq!(
            p.server.send(&cx, route, &packet, clock(&cx), || true),
            Err(Error::Expired)
        );
        assert_eq!(p.server.usage().retained_send_records, 0);
        let mut checks = 0;
        assert_eq!(
            p.server.send(&cx, route, &packet, clock(&cx) + 10000, || {
                checks += 1;
                checks == 1
            }),
            Err(Error::Unauthorized)
        );
        assert_eq!(checks, 2);
        assert!(p.server.is_closed());
        assert_eq!(
            p.client
                .receive(&cx, || true, |_, _| panic!("unauthorized record")),
            Ok(0)
        );
    });
}
#[test]
fn backpressure_retains_original_record_until_retry_and_native_acks_release_batch() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let policy = Policy {
            retained_send_records: 1,
            ..Policy::default()
        };
        let mut p = pair(&cx, policy).await;
        let packet = recovery_packet(2, 900);
        let route = Route::Stream(p.host_routes[1]);
        p.server
            .send(&cx, route, &packet, clock(&cx) + 2_000_000, || true)
            .unwrap();
        assert_eq!(
            p.server
                .send(&cx, route, &packet, clock(&cx) + 2_000_000, || true),
            Err(Error::Backpressure)
        );
        assert_eq!(p.server.usage().retained_send_records, 1);
        let mut count = 0;
        for _ in 0..500 {
            drive(&cx, &mut p).await;
            p.client
                .receive(
                    &cx,
                    || true,
                    |_, b| {
                        assert_eq!(b, packet);
                        count += 1;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if count == 1 && p.server.usage().retained_send_records == 0 {
                break;
            }
        }
        assert_eq!(count, 1);
        assert_eq!(p.server.usage().retained_send_records, 0);
        p.server
            .send(&cx, route, &packet, clock(&cx) + 2_000_000, || true)
            .unwrap();
        for _ in 0..500 {
            drive(&cx, &mut p).await;
            p.client
                .receive(
                    &cx,
                    || true,
                    |_, b| {
                        assert_eq!(b, packet);
                        count += 1;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if count == 2 {
                break;
            }
        }
        assert_eq!(count, 2);
    });
}
#[test]
fn stalled_complete_record_expires_without_a_new_packet() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(
            &cx,
            Policy {
                record_lifetime_micros: 100_000,
                ..Policy::default()
            },
        )
        .await;
        let packet = recovery_packet(2, 100);
        p.server
            .send(
                &cx,
                Route::Stream(p.host_routes[1]),
                &packet,
                clock(&cx) + 2_000_000,
                || true,
            )
            .unwrap();
        let mut offered = false;
        for _ in 0..100 {
            drive(&cx, &mut p).await;
            p.client
                .receive(
                    &cx,
                    || true,
                    |_, b| {
                        assert_eq!(b, packet);
                        offered = true;
                        Ok(Disposition::Blocked)
                    },
                )
                .unwrap();
            if offered {
                break;
            }
        }
        assert!(offered);
        asupersync::time::sleep(cx.now(), Duration::from_millis(120)).await;
        assert_eq!(
            p.client.receive(&cx, || true, |_, _| panic!("expired")),
            Err(Error::Stream(fr_wire::stream::StreamError::Expired))
        );
        assert!(p.client.is_closed());
        assert_eq!(p.client.usage().framed_capacity, 0);
    });
}
#[test]
fn cancelling_or_dropping_a_live_io_wait_prevents_connection_reuse() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (c, s) = support::native_pair(&cx, "localhost", ALPN).await;
        let mut client = QuicRecords::new(c.unwrap(), &cx, &[], &[], Policy::default()).unwrap();
        let mut server = QuicRecords::new(s.unwrap(), &cx, &[], &[], Policy::default()).unwrap();
        // Cancellation of an I/O future must not permit a later call to silently
        // resume partially submitted connection work. Wait until actually pending.
        let mut parked = false;
        for _ in 0..20 {
            let mut future = pin!(client.drive(&cx, Duration::from_millis(100), || true));
            parked = std::future::poll_fn(|task| match future.as_mut().poll(task) {
                Poll::Pending => Poll::Ready(true),
                Poll::Ready(result) => {
                    result.unwrap();
                    Poll::Ready(false)
                }
            })
            .await;
            if parked {
                break;
            }
        }
        assert!(
            parked,
            "a silent native connection must reach a pending wait"
        );
        assert!(client.is_closed());
        assert_eq!(
            client.drive(&cx, Duration::from_millis(1), || true).await,
            Err(Error::Closed)
        );
        cx.cancel_fast(CancelKind::User);
        assert_eq!(
            server.receive(&cx, || true, |_, _| Ok(Disposition::Consumed)),
            Err(Error::Cancelled)
        );
        assert!(server.is_closed());
    });
}
#[test]
fn hostname_and_application_protocol_are_actually_verified() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (bad, _) = support::native_pair(&cx, "not-localhost.invalid", ALPN).await;
        assert!(
            bad.is_err(),
            "the real certificate SAN must reject a different DNS name"
        );
        let (c, s) = support::native_pair(&cx, "localhost", b"different-protocol/0").await;
        assert_eq!(
            QuicRecords::new(c.unwrap(), &cx, &[], &[], Policy::default()).unwrap_err(),
            Error::Alpn
        );
        drop(s);
    });
}
fn channel(route: Route) -> Channel {
    match route {
        Route::Datagram(_) => Channel::Video,
        Route::Stream(s) => match s.kind {
            0x32 => Channel::Recovery,
            0x37 => Channel::MediaConfig,
            0x35 => Channel::Control,
            _ => panic!("unexpected route"),
        },
    }
}
#[test]
fn production_media_packetizer_and_receiver_run_over_native_quic() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap();
        let bindings = MediaBindings::new(1, 2, 3, 4).unwrap();
        let epoch = MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        };
        let mut send = SendCache::new(limits, bindings, epoch, SendPolicy::default()).unwrap();
        let mut recv = ReceivePipeline::new(
            ReceiveConfig {
                limits,
                bindings,
                epoch,
                policy: ReceivePolicy::default(),
            },
            MediaBudget::new(limits.protocol()).unwrap(),
        )
        .unwrap();
        recv.decoder_configured(clock(&cx)).unwrap();
        let mut delivered = 0;
        for frame in 0_u64..12 {
            let data: Vec<_> = (0_u8..=255)
                .cycle()
                .take(4000 + usize::try_from(frame).unwrap() * 5)
                .collect();
            let now = clock(&cx);
            send.push(
                Progress {
                    descriptor: FrameDescriptor {
                        frame,
                        total_bytes: u32::try_from(data.len()).unwrap(),
                        stride: limits.fragment_stride(),
                        capture_micros: now,
                        reference: frame.checked_sub(1),
                    },
                    observed_micros: now,
                    observation: SourceObservation::Captured,
                    pipeline: PipelineState::Running,
                },
                data.clone(),
                if frame == 0 {
                    DeliveryMode::Recovery
                } else {
                    DeliveryMode::Datagrams
                },
                now,
            )
            .unwrap();
            let mut out = [0; 1150];
            let mut pending = send.next_packet(clock(&cx), &mut out).unwrap();
            for _ in 0..500 {
                if let Some(offer) = pending {
                    let route = match offer.channel() {
                        Channel::Video => Route::Datagram(p.video),
                        Channel::Recovery => Route::Stream(p.host_routes[1]),
                        Channel::MediaConfig => Route::Stream(p.host_routes[0]),
                        Channel::Control => panic!("host original control"),
                    };
                    match p.server.send(
                        &cx,
                        route,
                        &out[..offer.byte_len()],
                        offer.send_by_micros(),
                        || true,
                    ) {
                        Ok(()) => pending = send.next_packet(clock(&cx), &mut out).unwrap(),
                        Err(Error::Backpressure) => (),
                        Err(e) => panic!("send failed: {e:?}"),
                    }
                }
                drive(&cx, &mut p).await;
                p.client
                    .receive(
                        &cx,
                        || true,
                        |route, bytes| {
                            recv.receive(channel(route), bytes, clock(&cx)).unwrap();
                            Ok(Disposition::Consumed)
                        },
                    )
                    .unwrap();
                if let Some(picture) = recv.take_decodable(clock(&cx)).unwrap() {
                    assert_eq!(picture.descriptor().frame, frame);
                    assert_eq!(picture.bytes(), data);
                    // This is byte-preservation evidence, not a codec claim.
                    recv.acknowledge_decode(&picture, true, clock(&cx)).unwrap();
                    delivered += 1;
                    break;
                }
            }
            assert_eq!(delivered, frame + 1);
            assert!(pending.is_none());
        }
        assert_eq!(delivered, 12);
        assert_eq!(recv.budget_usage().pictures, 0);
    });
}
#[test]
fn datagram_cap_rejects_before_native_fatal_path_and_delivers_exact_boundary() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let l = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap();
        let mut bytes = vec![0; 1150];
        let data = vec![19; 1077];
        let size = fr_wire::encode_fragment(
            fr_wire::Fragment {
                descriptor: FrameDescriptor {
                    frame: 1,
                    reference: Some(0),
                    total_bytes: 1077,
                    stride: 1077,
                    capture_micros: 0,
                },
                index: 0,
                bytes: &data,
            },
            1,
            &l,
            &mut bytes,
        )
        .unwrap();
        assert_eq!(size, 1150);
        let mut over = bytes.clone();
        over.push(0);
        assert_eq!(
            p.server.send(
                &cx,
                Route::Datagram(p.video),
                &over,
                clock(&cx) + 100_000,
                || true
            ),
            Err(Error::TooLarge)
        );
        assert!(!p.server.is_closed());
        p.server
            .send(
                &cx,
                Route::Datagram(p.video),
                &bytes,
                clock(&cx) + 100_000,
                || true,
            )
            .unwrap();
        let mut got = false;
        for _ in 0..100 {
            drive(&cx, &mut p).await;
            p.client
                .receive(
                    &cx,
                    || true,
                    |_, b| {
                        assert_eq!(b, bytes);
                        got = true;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if got {
                break;
            }
        }
        assert!(got);
        // A record encoder/decoder still validates message-specific semantics.
        let small = recovery_packet(2, 10);
        let l = MediaLimits::new(ProtocolLimits::ABSOLUTE, 65536, 16384, 64).unwrap();
        assert_eq!(
            decode_recovery(
                Record::decode(&small, &l, 2, Channel::Recovery).unwrap(),
                &l
            )
            .unwrap()
            .bytes
            .len(),
            10
        );
    });
}
