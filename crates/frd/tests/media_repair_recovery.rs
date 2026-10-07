#![cfg(target_os = "linux")]
//! Real TLS/UDP repair dispatch with synthetic encoded bytes. This tests transport
//! and authority ownership, not HEVC decoding or live-tailnet qualification.
#[path = "../../fr-transport/tests/support/mod.rs"]
#[allow(dead_code)]
mod net;

use asupersync::{cx::Cx, time::sleep, types::Budget};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{CodecConfigurationGeneration, RecoveryGeneration, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId, FrameKind},
    delivery::{DeliveryError, MediaBindings, MediaEpoch, SendError, SendPolicy},
};
use fr_transport::quic::{self, Disposition, Policy, Route, StreamRoute};
use fr_wire::{Channel, MediaLimits, RepairRange};
use frd::{
    media::{self, ObservationControl, Subscription, host_now},
    media_egress::{Admission, Egress, Lane, Progress},
    media_quic::{Error, QuicEgress, RepairAdmission, Routes},
};
use std::time::Duration;

fn limits() -> MediaLimits {
    MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16_384, 64).unwrap()
}
fn bindings() -> MediaBindings {
    MediaBindings::new(1, 2, 3, 4).unwrap()
}
fn repair() -> Vec<u8> {
    let mut bytes = vec![0; 1150];
    let n = fr_wire::encode_repair(
        FrameId::FIRST.next().unwrap().as_raw(),
        &[RepairRange { start: 0, end: 1 }],
        1,
        bindings().for_channel(Channel::Control),
        &limits(),
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}
fn sender(
    cx: &Cx,
    pair: &net::Pair,
    final_offer: bool,
    recovery_horizon_micros: u64,
) -> (QuicEgress, ObservationControl, u64) {
    let now = host_now(cx).unwrap();
    let mut authority = SessionAuthority::new(
        RemoteSessionId::from_raw(9),
        AuthorityPolicy::plan_defaults(),
    );
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(now).unwrap();
    authority.mark_view_ready(now).unwrap();
    let control = ObservationControl::new(cx.clone(), authority).unwrap();
    let policy = SendPolicy {
        recovery_horizon_micros,
        ..SendPolicy::default()
    };
    let mut egress = Egress::new(
        Subscription::new(
            control.clone(),
            limits(),
            bindings(),
            MediaEpoch {
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
            },
            policy,
        )
        .unwrap(),
    );
    let make = |frame, kind| {
        EncodedAccessUnit::new(
            &ProtocolLimits::ABSOLUTE,
            frame,
            kind,
            CodecConfigurationGeneration::INITIAL,
            now.as_micros(),
            vec![7; 128],
        )
        .unwrap()
    };
    egress
        .enqueue(make(
            FrameId::FIRST,
            FrameKind::Idr {
                recovery: RecoveryGeneration::INITIAL,
            },
        ))
        .unwrap();
    // A bounded synthetic bootstrap sink; the repair below uses actual QUIC.
    for _ in 0..3 {
        let progress = egress
            .transmit(Lane::Original, |_, _, guard| {
                guard().unwrap();
                Ok::<_, ()>(Admission::Accepted)
            })
            .unwrap();
        if progress == Progress::Idle {
            break;
        }
    }
    assert!(egress.pending().is_none());
    egress
        .enqueue(make(
            FrameId::FIRST.next().unwrap(),
            FrameKind::Predicted {
                references: FrameId::FIRST,
            },
        ))
        .unwrap();
    if final_offer {
        // Admit only its announcement. Preparing the one final fragment does
        // not admit it, even though the packetizer now considers it complete.
        egress
            .transmit(Lane::Original, |offer, _, guard| {
                assert_eq!(offer.channel(), Channel::MediaConfig);
                guard().unwrap();
                Ok::<_, ()>(Admission::Accepted)
            })
            .unwrap();
        egress
            .transmit(Lane::Original, |offer, _, guard| {
                assert_eq!(offer.channel(), Channel::Video);
                guard().unwrap();
                Ok::<_, ()>(Admission::Backpressure)
            })
            .unwrap();
        assert!(egress.pending().is_some());
    }
    let routes = Routes::new(
        bindings(),
        pair.host_routes[0],
        pair.host_routes[1],
        pair.video,
        pair.host_routes[2],
    )
    .unwrap();
    (
        QuicEgress::new(egress, routes),
        control,
        now.as_micros() + policy.reference_horizon_micros,
    )
}
async fn after(cx: &Cx, until: u64) {
    while net::clock(cx) <= until {
        let remaining = until.saturating_sub(net::clock(cx)).saturating_add(1_000);
        sleep(cx.now(), Duration::from_micros(remaining)).await;
    }
}
async fn dispatch(
    cx: &Cx,
    pair: &mut net::Pair,
    sender: &mut QuicEgress,
    control: &ObservationControl,
    bytes: &[u8],
) -> Result<(), quic::Error> {
    let route = pair.host_routes[2];
    let until = net::clock(cx) + 1_000_000;
    pair.client.send(
        cx,
        Route::Stream(StreamRoute {
            outbound: true,
            ..route
        }),
        bytes,
        until,
        || true,
    )?;
    loop {
        assert!(net::clock(cx) < until, "repair was not dispatched");
        net::drive(cx, pair).await;
        let consumed = pair.server.receive_ready(
            cx,
            || control.check().is_ok(),
            |r| r == Route::Stream(route),
            |r, bytes| {
                sender
                    .repair(r, bytes)
                    .map(|_| Disposition::Consumed)
                    .map_err(|_| ())
            },
        )?;
        if consumed != 0 {
            return Ok(());
        }
    }
}

#[test]
fn expired_repair_dispatch_preserves_quic_but_fences_the_original_view() {
    let rt = net::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        for final_offer in [false, true] {
            let mut pair = net::pair(&cx, Policy::default()).await;
            let (mut sender, control, until) = sender(&cx, &pair, final_offer, 2_000_000);
            after(&cx, until).await;
            dispatch(&cx, &mut pair, &mut sender, &control, &repair())
                .await
                .unwrap();
            assert!(!pair.server.is_closed());
            assert!(!sender.is_closed());
            assert!(!control.view_ready().unwrap());
            assert!(sender.pending().is_none());
            assert_eq!(sender.cache_usage().bytes, 0);
            let recovery_until = sender.next_deadline().unwrap();
            assert!(recovery_until.as_micros() > net::clock(&cx));
            for _ in 0..3 {
                assert_eq!(
                    sender.repair(Route::Stream(pair.host_routes[2]), &repair()),
                    Ok(RepairAdmission::Refused)
                );
                assert_eq!(sender.next_deadline(), Some(recovery_until));
            }
            assert_eq!(
                sender.transmit(&cx, &mut pair.server, Lane::Original),
                Err(Error::Media(media::Error::Send(SendError::NeedsRecovery)))
            );
            assert!(!pair.server.is_closed());
            assert_eq!(pair.server.usage().retained_send_records, 0);
        }
    });
}

#[test]
fn malformed_repair_is_still_terminal_after_local_media_expiry() {
    let rt = net::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut pair = net::pair(&cx, Policy::default()).await;
        let (mut sender, control, until) = sender(&cx, &pair, true, 2_000_000);
        after(&cx, until).await;
        let mut bytes = repair();
        let n = bytes.len();
        bytes[n - 4..].copy_from_slice(&0_u32.to_be_bytes()); // empty range
        assert_eq!(
            dispatch(&cx, &mut pair, &mut sender, &control, &bytes).await,
            Err(quic::Error::Handler)
        );
        assert!(pair.server.is_closed());
    });
}

#[test]
fn wrong_route_does_not_consume_the_pending_offer_or_start_recovery() {
    let rt = net::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let pair = net::pair(&cx, Policy::default()).await;
        let (mut sender, control, until) = sender(&cx, &pair, true, 2_000_000);
        let pending = sender.pending().unwrap().clone();
        let usage = sender.cache_usage();
        after(&cx, until).await;
        assert_eq!(
            sender.repair(Route::Stream(pair.host_routes[0]), &repair()),
            Err(Error::InvalidRoutes)
        );
        assert_eq!(sender.pending(), Some(&pending));
        assert_eq!(sender.cache_usage(), usage);
        assert!(control.view_ready().unwrap());
        control.revoke();
        assert!(sender.repair(Route::Stream(pair.host_routes[2]), &repair()).is_err());
        assert!(sender.tick().is_err());
        assert!(sender.is_closed());
    });
}

#[test]
fn valid_repairs_cannot_extend_the_terminal_recovery_deadline() {
    let rt = net::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let pair = net::pair(&cx, Policy::default()).await;
        let (mut sender, _, until) = sender(&cx, &pair, true, 100_000);
        after(&cx, until).await;
        let route = Route::Stream(pair.host_routes[2]);
        assert_eq!(sender.repair(route, &repair()), Ok(RepairAdmission::Refused));
        let recovery_until = sender.next_deadline().unwrap();
        after(&cx, recovery_until.as_micros()).await;
        assert_eq!(
            sender.repair(route, &repair()),
            Err(Error::Media(media::Error::Send(SendError::Delivery(
                DeliveryError::RecoveryExpired,
            ))))
        );
        assert!(sender.tick().is_err());
        assert!(sender.is_closed());
    });
}
