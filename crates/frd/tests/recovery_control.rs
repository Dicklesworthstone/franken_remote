#![cfg(target_os = "linux")]
//! Real TLS/UDP and completed media attachments. Decode completion is simulated;
//! these tests do not qualify HEVC, native presentation or live-tailnet admission.
#[path = "../../fr-transport/tests/support/mod.rs"]
#[allow(dead_code)]
mod net;
use asupersync::cx::Cx;
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_media::delivery::{MediaBudget, ReceivePipeline, ReceivePolicy};
use fr_transport::quic::{self, *};
use fr_wire::{
    Channel, RecoveryChunk,
    input::{InputDelivery as T, InputDirection as D},
    negotiation::ControlBinding,
    recovery_request::{self as wire, Reason},
};
use frd::media_quic::{
    NegotiatedMedia,
    recovery::{Error as RecoveryError, Receiver, State},
};

#[path = "support/recovery_link.rs"]
mod recovery_link;
use recovery_link::*;

fn decode_bootstrap(
    receiver: &mut ReceivePipeline,
    watcher: &mut Receiver,
    media: &NegotiatedMedia,
    stamp: u64,
) {
    receiver.decoder_configured(stamp).unwrap();
    let mut bytes = [0; 1150];
    let n = fr_wire::encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 4,
            offset: 0,
            capture_micros: stamp,
            bytes: b"test",
        },
        media.bindings().for_channel(Channel::Recovery),
        &media.limits(),
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(Channel::Recovery, &bytes[..n], stamp)
        .unwrap();
    let picture = receiver.take_decodable(stamp).unwrap().unwrap();
    let decoded = receiver.complete_decode(&picture, stamp).unwrap();
    watcher.observe_decoded(&decoded).unwrap();
}
fn corrupt_decoder(receiver: &mut ReceivePipeline, media: &NegotiatedMedia, stamp: u64) {
    let mut bytes = [0; 1150];
    let descriptor = fr_wire::FrameDescriptor {
        frame: 1,
        total_bytes: 4,
        stride: 4,
        capture_micros: stamp,
        reference: Some(0),
    };
    let n = fr_wire::encode_fragment(
        fr_wire::Fragment {
            descriptor,
            index: 0,
            bytes: b"next",
        },
        media.bindings().for_channel(Channel::Video),
        &media.limits(),
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(Channel::Video, &bytes[..n], stamp)
        .unwrap();
    let picture = receiver.take_decodable(stamp).unwrap().unwrap();
    assert!(receiver.acknowledge_decode(&picture, false, stamp).is_err());
}
#[test]
fn actual_failed_receiver_reports_once_on_the_original_control_pair() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (_, media, mut receiver) = link.media(&cx).await;
        let mut watcher = media
            .recovery_receiver(&link.c, link.cr, parent(), &receiver)
            .unwrap();
        let stamp = net::clock(&cx);
        decode_bootstrap(&mut receiver, &mut watcher, &media, stamp);
        assert_eq!(
            watcher
                .service(&cx, &mut link.c, &mut receiver, || true)
                .unwrap(),
            State::Receiving
        );
        corrupt_decoder(&mut receiver, &media, net::clock(&cx));
        // Drain handshake ACKs before giving the request the sole critical slot.
        let until = net::clock(&cx) + 1_000_000;
        let mut got = 0;
        while got == 0 {
            assert!(net::clock(&cx) < until);
            let state = watcher
                .service(&cx, &mut link.c, &mut receiver, || true)
                .unwrap();
            assert!(matches!(state, State::Pending | State::Requested));
            link.drive(&cx).await;
            link.h
                .receive_ready(
                    &cx,
                    || true,
                    |r| r == Route::Stream(link.hr.inbound),
                    |_, b| {
                        let r = wire::decode(
                            b,
                            binding(7),
                            &ProtocolLimits::ABSOLUTE,
                            D::ViewerToHost,
                            T::Reliable,
                        )
                        .unwrap();
                        assert_eq!(r.reason, Reason::DecodeFailed);
                        assert_eq!(r.last_useful_frame, Some(0));
                        got += 1;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        let original = watcher.next_deadline().unwrap();
        for _ in 0..8 {
            assert_eq!(
                watcher
                    .service(&cx, &mut link.c, &mut receiver, || true)
                    .unwrap(),
                State::Requested
            );
            link.drive(&cx).await;
        }
        link.h
            .receive_ready(
                &cx,
                || true,
                |_| true,
                |_, _| {
                    got += 1;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        assert_eq!(got, 1);
        assert_eq!(watcher.next_deadline(), Some(original));
        assert!(!link.c.is_closed());
        assert!(!link.h.is_closed());
    });
}
#[test]
fn missing_capability_wrong_role_parent_and_receiver_configuration_refuse() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, false).await;
        let (_, media, receiver) = link.media(&cx).await;
        assert!(matches!(
            media.recovery_receiver(&link.c, link.cr, parent(), &receiver),
            Err(RecoveryError::NotNegotiated)
        ));
        let mut link = Link::new(&cx, true).await;
        let (host, media, mut receiver) = link.media(&cx).await;
        assert!(
            host.recovery_receiver(&link.h, link.hr, parent(), &receiver)
                .is_err()
        );
        let parent = ControlBinding {
            remote_session: RemoteSessionId::from_raw(99),
            ..parent()
        };
        assert!(
            media
                .recovery_receiver(&link.c, link.cr, parent, &receiver)
                .is_err()
        );
        receiver.close();
        assert!(
            media
                .recovery_receiver(&link.c, link.cr, self::parent(), &receiver)
                .is_err()
        );
        assert!(!link.c.is_closed());
    });
}
#[test]
fn authority_loss_or_a_foreign_connection_cannot_submit_recovery() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (_, media, mut receiver) = link.media(&cx).await;
        let mut watcher = media
            .recovery_receiver(&link.c, link.cr, parent(), &receiver)
            .unwrap();
        let mut foreign = Link::new(&cx, true).await;
        let before = foreign.c.usage();
        assert_eq!(
            watcher.service(&cx, &mut foreign.c, &mut receiver, || true),
            Err(RecoveryError::WrongBinding)
        );
        assert_eq!(foreign.c.usage(), before);
        assert!(!foreign.c.is_closed());
        assert_eq!(watcher.state(), State::Closed);
        let mut watcher = media
            .recovery_receiver(&link.c, link.cr, parent(), &receiver)
            .unwrap();
        assert_eq!(
            watcher.service(&cx, &mut link.c, &mut receiver, || false),
            Err(RecoveryError::Transport(quic::Error::Unauthorized))
        );
        assert_eq!(
            watcher.service(&cx, &mut link.c, &mut receiver, || true),
            Err(RecoveryError::Closed)
        );
    });
}

async fn occupy_control(link: &mut Link, cx: &Cx) -> Vec<u8> {
    let mut record = vec![0; fr_wire::authority::MAX_AUTHORITY_BYTES];
    let n = fr_wire::authority::encode(
        fr_wire::authority::Message::Response {
            scope: fr_wire::authority::Scope::Observation,
            nonce: 123,
        },
        fr_wire::authority::Binding {
            channel: parent().id,
            session: parent().remote_session,
        },
        &ProtocolLimits::ABSOLUTE,
        &mut record,
        D::ViewerToHost,
        T::Reliable,
    )
    .unwrap();
    record.truncate(n);
    let until = net::clock(cx) + 1_000_000;
    loop {
        assert!(net::clock(cx) < until);
        match link
            .c
            .send(cx, Route::Stream(link.cr.outbound), &record, until, || true)
        {
            Ok(()) => return record,
            Err(quic::Error::Backpressure) => link.drive(cx).await,
            other => panic!("control admission: {other:?}"),
        }
    }
}
#[test]
fn backpressure_preserves_request_and_renewal_order_until_real_transport_credit_returns() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (_, media, mut receiver) = link.media(&cx).await;
        let mut watcher = media
            .recovery_receiver(&link.c, link.cr, parent(), &receiver)
            .unwrap();
        decode_bootstrap(&mut receiver, &mut watcher, &media, net::clock(&cx));
        let renewal = occupy_control(&mut link, &cx).await;
        corrupt_decoder(&mut receiver, &media, net::clock(&cx));
        let occupied = link.c.usage();
        assert_eq!(
            watcher
                .service(&cx, &mut link.c, &mut receiver, || true)
                .unwrap(),
            State::Pending
        );
        let original = watcher.next_deadline();
        for _ in 0..20 {
            assert_eq!(
                watcher
                    .service(&cx, &mut link.c, &mut receiver, || true)
                    .unwrap(),
                State::Pending
            );
            assert_eq!(link.c.usage(), occupied);
            assert_eq!(watcher.next_deadline(), original);
        }
        let mut kinds = Vec::new();
        let until = net::clock(&cx) + 1_000_000;
        while kinds.len() < 2 {
            assert!(net::clock(&cx) < until);
            link.drive(&cx).await;
            link.h
                .receive_ready(
                    &cx,
                    || true,
                    |r| r == Route::Stream(link.hr.inbound),
                    |_, b| {
                        if kinds.is_empty() {
                            assert_eq!(b, renewal);
                        } else {
                            assert_eq!(
                                wire::decode(
                                    b,
                                    binding(7),
                                    &ProtocolLimits::ABSOLUTE,
                                    D::ViewerToHost,
                                    T::Reliable
                                )
                                .unwrap()
                                .reason,
                                Reason::DecodeFailed
                            );
                        }
                        kinds.push(u16::from_be_bytes([b[6], b[7]]));
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            watcher
                .service(&cx, &mut link.c, &mut receiver, || true)
                .unwrap();
        }
        assert_eq!(kinds, [0x0016, 0x0036]);
        assert_eq!(watcher.next_deadline(), original);
    });
}
#[test]
fn waiting_for_credit_and_waiting_after_send_share_one_immutable_deadline() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for blocked in [false, true] {
            let mut link = Link::new(&cx, true).await;
            let (_, media, _) = link.media(&cx).await;
            let cfg = media
                .receiver_config(
                    &link.c,
                    ReceivePolicy {
                        recovery_budget_micros: 100_000,
                        ..ReceivePolicy::default()
                    },
                )
                .unwrap();
            let mut receiver =
                ReceivePipeline::new(cfg, MediaBudget::new(cfg.limits.protocol()).unwrap())
                    .unwrap();
            let mut watcher = media
                .recovery_receiver(&link.c, link.cr, parent(), &receiver)
                .unwrap();
            decode_bootstrap(&mut receiver, &mut watcher, &media, net::clock(&cx));
            if blocked {
                occupy_control(&mut link, &cx).await;
            }
            corrupt_decoder(&mut receiver, &media, net::clock(&cx));
            let until = net::clock(&cx) + 500_000;
            loop {
                assert!(net::clock(&cx) < until);
                let state = watcher
                    .service(&cx, &mut link.c, &mut receiver, || true)
                    .unwrap();
                if blocked {
                    assert_eq!(state, State::Pending);
                    break;
                }
                if state == State::Requested {
                    break;
                }
                link.drive(&cx).await;
            }
            let original = watcher.next_deadline().unwrap();
            let until = asupersync::types::Time::from_nanos((original + 1000) * 1000);
            asupersync::time::sleep_until(until).await;
            assert_eq!(
                watcher.service(&cx, &mut link.c, &mut receiver, || true),
                Err(RecoveryError::Delivery(
                    fr_media::delivery::DeliveryError::RecoveryExpired
                ))
            );
            assert_eq!(watcher.state(), State::Closed);
            assert_eq!(
                watcher.service(&cx, &mut link.c, &mut receiver, || true),
                Err(RecoveryError::Closed)
            );
        }
    });
}
#[test]
fn malformed_media_and_replaced_receiver_never_become_new_requests() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for replace in [false, true] {
            let mut link = Link::new(&cx, true).await;
            let (_, media, mut receiver) = link.media(&cx).await;
            let mut watcher = media
                .recovery_receiver(&link.c, link.cr, parent(), &receiver)
                .unwrap();
            decode_bootstrap(&mut receiver, &mut watcher, &media, net::clock(&cx));
            if replace {
                receiver
                    .replace(
                        fr_media::delivery::MediaEpoch {
                            configuration: binding(7).configuration,
                            recovery: binding(7).recovery.next().unwrap(),
                        },
                        fr_media::delivery::MediaBindings::new(20, 21, 22, 23).unwrap(),
                        net::clock(&cx),
                    )
                    .unwrap();
            } else {
                assert!(
                    receiver
                        .receive(Channel::Video, b"bad", net::clock(&cx))
                        .is_err()
                );
            }
            let usage = link.c.usage();
            assert!(
                watcher
                    .service(&cx, &mut link.c, &mut receiver, || true)
                    .is_err()
            );
            assert_eq!(link.c.usage(), usage);
            assert_eq!(watcher.next_deadline(), None);
        }
    });
}
