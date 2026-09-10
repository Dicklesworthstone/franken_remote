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
        Route::Stream(s) => match s.messages {
            Messages::Exact(0x32) => Channel::Recovery,
            Messages::Exact(0x37) => Channel::MediaConfig,
            Messages::Exact(0x35) => Channel::Control,
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
                if let Some(offer) = pending.as_ref() {
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

fn record(binding: u32, kind: u16, length: usize, tag: u8) -> Vec<u8> {
    let mut bytes = vec![tag; length];
    bytes[..6].copy_from_slice(b"FRD0\0\0");
    bytes[6..8].copy_from_slice(&kind.to_be_bytes());
    bytes[8..12].fill(0);
    bytes[12..16].copy_from_slice(&u32::try_from(length - 24).unwrap().to_be_bytes());
    bytes[16..20].copy_from_slice(&binding.to_be_bytes());
    bytes[20..24].fill(0);
    bytes
}
struct InputPair {
    client: QuicRecords,
    server: QuicRecords,
    actions: StreamRoute,
    results: StreamRoute,
    pointer: DatagramRoute,
    bulk: StreamRoute,
}
async fn input_pair(cx: &Cx, policy: Policy) -> InputPair {
    let (client, server) = support::native_pair_with_windows(
        cx,
        "localhost",
        ALPN,
        policy.stream_window,
        policy.connection_window,
    )
    .await;
    let (mut client, mut server) = (client.unwrap(), server.unwrap());
    let input = client.connection_mut().open_uni_stream(cx).unwrap();
    let result = server.connection_mut().open_uni_stream(cx).unwrap();
    let bulk = server.connection_mut().open_uni_stream(cx).unwrap();
    let actions = StreamRoute {
        stream: input,
        binding: 7,
        messages: Messages::InputActions,
        priority: Priority::Critical,
        outbound: true,
        maximum: 512,
    };
    let results = StreamRoute {
        stream: result,
        binding: 7,
        messages: Messages::Exact(0x0048),
        priority: Priority::Critical,
        outbound: true,
        maximum: 512,
    };
    let bulk = StreamRoute {
        stream: bulk,
        binding: 2,
        messages: Messages::Exact(0x0032),
        priority: Priority::Bulk,
        outbound: true,
        maximum: usize::try_from(policy.stream_window).unwrap(),
    };
    let pointer = DatagramRoute {
        binding: 7,
        kind: 0x0042,
        outbound: true,
    };
    let client_routes = [
        actions,
        StreamRoute {
            outbound: false,
            ..results
        },
        StreamRoute {
            outbound: false,
            ..bulk
        },
    ];
    let server_routes = client_routes.map(|r| StreamRoute {
        outbound: !r.outbound,
        ..r
    });
    InputPair {
        client: QuicRecords::new(client, cx, &client_routes, &[pointer], policy).unwrap(),
        server: QuicRecords::new(
            server,
            cx,
            &server_routes,
            &[DatagramRoute {
                outbound: false,
                ..pointer
            }],
            policy,
        )
        .unwrap(),
        actions,
        results,
        pointer,
        bulk,
    }
}
fn input_drive<'a>(
    cx: &'a Cx,
    p: &'a mut InputPair,
) -> std::pin::Pin<Box<impl Future<Output = ()> + 'a>> {
    Box::pin(async move {
        let (c, s) = Box::pin(support::both(
            p.client.drive(cx, Duration::from_millis(1), || true),
            p.server.drive(cx, Duration::from_millis(1), || true),
        ))
        .await;
        c.unwrap();
        s.unwrap();
    })
}
#[test]
fn mixed_actions_keep_one_ordered_stream_and_share_binding_with_results_and_pointer() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = input_pair(&cx, Policy::default()).await;
        // The transport checks class/framing; action payload semantics stay in
        // the fr-wire action and HeldState codecs. Distinct markers expose
        // cross-kind stream reordering, including release-only reconciliation.
        let frames: Vec<_> = [0x40, 0x41, 0x45, 0x46, 0x44, 0x43, 0x47, 0x40]
            .iter()
            .enumerate()
            .map(|(i, kind)| record(7, *kind, 128, u8::try_from(i).unwrap()))
            .collect();
        for bytes in &frames {
            p.client
                .send(
                    &cx,
                    Route::Stream(p.actions),
                    bytes,
                    clock(&cx) + 2_000_000,
                    || true,
                )
                .unwrap();
        }
        for forbidden in [0x42, 0x48, 0x49, 0x32, 0xffff] {
            assert_eq!(
                p.client.send(
                    &cx,
                    Route::Stream(p.actions),
                    &record(7, forbidden, 128, 0),
                    clock(&cx) + 2_000_000,
                    || true
                ),
                Err(Error::WrongRoute)
            );
        }
        let pointer = record(7, 0x42, 120, 19);
        p.client
            .send(
                &cx,
                Route::Datagram(p.pointer),
                &pointer,
                clock(&cx) + 2_000_000,
                || true,
            )
            .unwrap();
        let response = record(7, 0x48, 74, 23);
        p.server
            .send(
                &cx,
                Route::Stream(p.results),
                &response,
                clock(&cx) + 2_000_000,
                || true,
            )
            .unwrap();
        let mut received = vec![];
        let (mut got_pointer, mut got_result) = (false, false);
        for _ in 0..200 {
            input_drive(&cx, &mut p).await;
            p.server
                .receive(
                    &cx,
                    || true,
                    |r, b| {
                        match r {
                            Route::Stream(_) => received.push(b.to_vec()),
                            Route::Datagram(_) => {
                                assert_eq!(b, pointer);
                                got_pointer = true;
                            }
                        }
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            p.client
                .receive(
                    &cx,
                    || true,
                    |_, b| {
                        assert_eq!(b, response);
                        got_result = true;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if received.len() == frames.len() && got_pointer && got_result {
                break;
            }
        }
        assert_eq!(received, frames);
        assert!(got_pointer && got_result);
    });
}
#[test]
fn exhausted_bulk_pool_cannot_consume_critical_storage_and_critical_runs_first() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = input_pair(
            &cx,
            Policy {
                retained_send_records: 1,
                critical_send_records: 1,
                ..Policy::default()
            },
        )
        .await;
        let bulk = record(2, 0x32, 32000, 8);
        let result = record(7, 0x48, 74, 9);
        p.server
            .send(
                &cx,
                Route::Stream(p.bulk),
                &bulk,
                clock(&cx) + 2_000_000,
                || true,
            )
            .unwrap();
        assert_eq!(
            p.server.send(
                &cx,
                Route::Stream(p.bulk),
                &bulk,
                clock(&cx) + 2_000_000,
                || true
            ),
            Err(Error::Backpressure)
        );
        p.server
            .send(
                &cx,
                Route::Stream(p.results),
                &result,
                clock(&cx) + 2_000_000,
                || true,
            )
            .unwrap();
        assert_eq!(
            p.server.send(
                &cx,
                Route::Stream(p.results),
                &result,
                clock(&cx) + 2_000_000,
                || true
            ),
            Err(Error::Backpressure)
        );
        assert_eq!(p.server.usage().critical_send_records, 1);
        assert_eq!(p.server.usage().retained_send_records, 2);
        let mut kinds = vec![];
        for _ in 0..400 {
            input_drive(&cx, &mut p).await;
            p.client
                .receive(
                    &cx,
                    || true,
                    |_, b| {
                        kinds.push(u16::from_be_bytes([b[6], b[7]]));
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if kinds.len() == 2 {
                break;
            }
        }
        assert_eq!(kinds, [0x48, 0x32]);
    });
}
#[test]
fn blocked_bulk_stream_cannot_pin_acknowledged_critical_storage() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = input_pair(
            &cx,
            Policy {
                stream_window: 1024,
                connection_window: 4096,
                critical_connection_credit: 512,
                critical_send_records: 1,
                ..Policy::default()
            },
        )
        .await;
        for tag in [11, 12, 13, 14] {
            p.server
                .send(
                    &cx,
                    Route::Stream(p.bulk),
                    &record(2, 0x32, 1024, tag),
                    clock(&cx) + 2_000_000,
                    || true,
                )
                .unwrap();
        }
        // The bulk consumer is unavailable: do not drain its native window.
        // That stream fills, but ACKed control storage must remain reusable.
        for _ in 0..40 {
            input_drive(&cx, &mut p).await;
            p.client
                .receive_ready(&cx, || true, |_| false, |_, _| panic!("blocked consumer"))
                .unwrap();
        }
        for tag in 0..8 {
            let result = record(7, 0x48, 74, tag);
            p.server
                .send(
                    &cx,
                    Route::Stream(p.results),
                    &result,
                    clock(&cx) + 2_000_000,
                    || true,
                )
                .unwrap();
            let mut received = false;
            for _ in 0..100 {
                input_drive(&cx, &mut p).await;
                p.client
                    .receive_ready(
                        &cx,
                        || true,
                        |route| {
                            route
                                == Route::Stream(StreamRoute {
                                    outbound: false,
                                    ..p.results
                                })
                        },
                        |route, b| {
                            if route
                                == Route::Stream(StreamRoute {
                                    outbound: false,
                                    ..p.results
                                })
                            {
                                assert_eq!(b, result);
                                received = true;
                                Ok(Disposition::Consumed)
                            } else {
                                Ok(Disposition::Blocked)
                            }
                        },
                    )
                    .unwrap();
                if received && p.server.usage().critical_send_records == 0 {
                    break;
                }
            }
            assert!(received);
            assert_eq!(p.server.usage().critical_send_records, 0);
            assert!(p.server.usage().retained_send_records > 0);
        }
    });
}
#[test]
fn critical_credit_is_reserved_in_the_actual_native_connection_window() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = input_pair(
            &cx,
            Policy {
                stream_window: 4096,
                connection_window: 4096,
                critical_connection_credit: 512,
                ..Policy::default()
            },
        )
        .await;
        let bulk = record(2, 0x32, 4096, 41);
        p.server
            .send(
                &cx,
                Route::Stream(p.bulk),
                &bulk,
                clock(&cx) + 2_000_000,
                || true,
            )
            .unwrap();
        // Drive packets and ACKs without application reads. Bulk may consume
        // 3584 bytes of actual connection credit, but never the reserved 512.
        for _ in 0..40 {
            input_drive(&cx, &mut p).await;
        }
        let result = record(7, 0x48, 74, 42);
        p.server
            .send(
                &cx,
                Route::Stream(p.results),
                &result,
                clock(&cx) + 2_000_000,
                || true,
            )
            .unwrap();
        for _ in 0..40 {
            input_drive(&cx, &mut p).await;
        }
        let mut kinds = vec![];
        // One synchronous drain cannot send new flow-credit updates. Thus an
        // already complete result proves reserved bytes reached the peer.
        p.client
            .receive(
                &cx,
                || true,
                |_, b| {
                    kinds.push(u16::from_be_bytes([b[6], b[7]]));
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        assert_eq!(kinds, [0x48]);
        for _ in 0..100 {
            input_drive(&cx, &mut p).await;
            p.client
                .receive(
                    &cx,
                    || true,
                    |_, b| {
                        assert_eq!(b, bulk);
                        kinds.push(0x32);
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if kinds.len() == 2 {
                break;
            }
        }
        assert_eq!(kinds, [0x48, 0x32]);
    });
}
#[test]
fn parallel_action_streams_and_wrong_initiators_are_refused_before_admission() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (client, server) = support::native_pair(&cx, "localhost", ALPN).await;
        let (mut client, mut server) = (client.unwrap(), server.unwrap());
        let first = client.connection_mut().open_uni_stream(&cx).unwrap();
        let second = client.connection_mut().open_uni_stream(&cx).unwrap();
        let route = StreamRoute {
            stream: first,
            binding: 7,
            messages: Messages::InputActions,
            priority: Priority::Critical,
            outbound: true,
            maximum: 512,
        };
        assert_eq!(
            QuicRecords::new(
                client,
                &cx,
                &[
                    route,
                    StreamRoute {
                        stream: second,
                        ..route
                    }
                ],
                &[],
                Policy::default()
            )
            .unwrap_err(),
            Error::InvalidPolicy
        );
        let wrong = server.connection_mut().open_uni_stream(&cx).unwrap();
        assert_eq!(
            QuicRecords::new(
                server,
                &cx,
                &[StreamRoute {
                    stream: wrong,
                    ..route
                }],
                &[],
                Policy::default()
            )
            .unwrap_err(),
            Error::InvalidPolicy
        );
    });
}

async fn bootstrap_connections(
    cx: &Cx,
) -> (QuicRecords, ControlRoutes, QuicRecords, ControlRoutes) {
    let (client, host) = support::native_pair(cx, "localhost", ALPN).await;
    let (client, c) = QuicRecords::bootstrap(client.unwrap(), cx, Policy::default()).unwrap();
    let (host, h) = QuicRecords::bootstrap(host.unwrap(), cx, Policy::default()).unwrap();
    (client, c, host, h)
}
fn startup_bytes(message: &fr_wire::negotiation::Message) -> Vec<u8> {
    let mut bytes = vec![0; fr_wire::negotiation::MAX_RECORD];
    let n = fr_wire::negotiation::encode(message, bytes.len(), &mut bytes).unwrap();
    bytes.truncate(n);
    bytes
}
fn startup_offer() -> fr_wire::negotiation::Offer {
    fr_wire::negotiation::Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: fr_wire::negotiation::Role::Observe,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities: vec![],
    }
}
async fn bootstrap_drive(cx: &Cx, client: &mut QuicRecords, host: &mut QuicRecords) {
    let (a, b) = Box::pin(support::both(
        client.drive(cx, Duration::from_millis(1), || true),
        host.drive(cx, Duration::from_millis(1), || true),
    ))
    .await;
    a.unwrap();
    b.unwrap();
}
#[test]
fn bootstrap_hello_and_bound_ack_use_the_same_authenticated_streams() {
    runtime().block_on(async {
        use fr_wire::negotiation::{self, Message};
        let cx = Cx::current().unwrap();
        let (mut client, c, mut host, h) = Box::pin(bootstrap_connections(&cx)).await;
        let identity = host.binding();
        let hello = Message::ClientHello(startup_offer());
        client
            .send(
                &cx,
                Route::Stream(c.outbound),
                &startup_bytes(&hello),
                clock(&cx) + 2_000_000,
                || true,
            )
            .unwrap();
        assert!(matches!(
            client.bind_control(&cx, c, 1, 4096, || true),
            Err(Error::Backpressure)
        ));
        let mut received = None;
        for _ in 0..100 {
            bootstrap_drive(&cx, &mut client, &mut host).await;
            host.receive(
                &cx,
                || true,
                |route, bytes| {
                    assert_eq!(route, Route::Stream(h.inbound));
                    received = Some(negotiation::decode(bytes, 4096, 0).unwrap());
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
            if received.is_some() {
                break;
            }
        }
        assert_eq!(received, Some(hello));
        let h2 = host.bind_control(&cx, h, 7, 4096, || true).unwrap();
        let c2 = client.bind_control(&cx, c, 7, 4096, || true).unwrap();
        assert_eq!(h2.inbound.stream, h.inbound.stream);
        assert!(host.is_bound_to(&identity));
        assert!(!host.has_route(Route::Stream(h.inbound)));
        assert!(matches!(
            host.bind_control(&cx, h, 8, 4096, || true),
            Err(Error::WrongRoute)
        ));
        let ack = Message::BindingAccepted { binding: 7 };
        client
            .send(
                &cx,
                Route::Stream(c2.outbound),
                &startup_bytes(&ack),
                clock(&cx) + 2_000_000,
                || true,
            )
            .unwrap();
        let mut received = None;
        for _ in 0..100 {
            bootstrap_drive(&cx, &mut client, &mut host).await;
            host.receive(
                &cx,
                || true,
                |route, bytes| {
                    assert_eq!(route, Route::Stream(h2.inbound));
                    received = Some(negotiation::decode(bytes, 4096, 7).unwrap());
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
            if received.is_some() {
                break;
            }
        }
        assert_eq!(received, Some(ack));
    });
}
#[test]
fn bootstrap_is_not_a_zero_bound_media_or_input_escape() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut client, c, host, _h) = Box::pin(bootstrap_connections(&cx)).await;
        for kind in [0x0040u16, 0x0032, 0x0034, 0x001c] {
            let mut bytes = vec![0u8; 24];
            bytes[..4].copy_from_slice(b"FRD0");
            bytes[6..8].copy_from_slice(&kind.to_be_bytes());
            assert!(matches!(
                client.send(
                    &cx,
                    Route::Stream(c.outbound),
                    &bytes,
                    clock(&cx) + 1_000_000,
                    || true
                ),
                Err(Error::WrongRoute)
            ));
        }
        assert_eq!(client.usage().retained_send_records, 0);
        assert_eq!(client.addresses().unwrap().0, host.addresses().unwrap().1);
        assert_eq!(
            client.role().unwrap(),
            asupersync::net::quic_native::StreamRole::Client
        );
        assert!(matches!(
            client.bind_control(&cx, c, 0, 4096, || true),
            Err(Error::WrongRoute)
        ));
        assert!(matches!(
            client.bind_control(&cx, c, 1, 4097, || true),
            Err(Error::WrongRoute)
        ));
        assert!(matches!(
            client.bind_control(&cx, c, 1, 4096, || false),
            Err(Error::Unauthorized)
        ));
        assert!(client.is_closed());
    });
}
#[test]
fn bootstrap_transition_cannot_relabel_an_incomplete_record() {
    runtime().block_on(async {
        use fr_wire::negotiation::{Capability, Message};
        let cx = Cx::current().unwrap();
        let (mut client, c, mut host, h) = Box::pin(bootstrap_connections(&cx)).await;
        let mut offer = startup_offer();
        offer.capabilities = (0..16)
            .map(|i| Capability {
                name: format!("c{i:02}{}", "x".repeat(60)),
                version: 1,
                required: false,
            })
            .collect();
        let bytes = startup_bytes(&Message::ClientHello(offer));
        assert!(bytes.len() > 900);
        client
            .send(
                &cx,
                Route::Stream(c.outbound),
                &bytes,
                clock(&cx) + 2_000_000,
                || true,
            )
            .unwrap();
        bootstrap_drive(&cx, &mut client, &mut host).await;
        host.receive(
            &cx,
            || true,
            |_, _| panic!("only first 900-byte prefix was staged"),
        )
        .unwrap();
        assert!(host.usage().framed_capacity > 0);
        assert!(matches!(
            host.bind_control(&cx, h, 9, 4096, || true),
            Err(Error::Backpressure)
        ));
        let mut complete = false;
        for _ in 0..100 {
            bootstrap_drive(&cx, &mut client, &mut host).await;
            host.receive(
                &cx,
                || true,
                |_, got| {
                    assert_eq!(got, bytes);
                    complete = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
            if complete {
                break;
            }
        }
        assert!(complete);
        host.bind_control(&cx, h, 9, 4096, || true).unwrap();
    });
}
