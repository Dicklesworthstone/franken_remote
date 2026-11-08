#![cfg(target_os = "linux")]
//! Real TLS/UDP and completed media attachments. Decode completion is simulated;
//! these tests do not qualify HEVC, native presentation or live-tailnet admission.
#[path = "support/executable.rs"]
mod executable;
#[path = "../../fr-transport/tests/support/mod.rs"]
#[allow(dead_code)]
mod net;
#[path = "support/worker.rs"]
#[allow(dead_code)]
mod worker_support;
use asupersync::cx::Cx;
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_media::delivery::{ReceivePipeline, ReceivePolicy};
use fr_transport::quic::*;
use fr_wire::{
    attachment::Ticket,
    decoder::{self, Binding},
    recovery_request as wire,
};
use frd::media_quic::{NegotiatedMedia, replacement::Replacement};
use std::time::Duration;
#[path = "support/recovery_link.rs"]
mod recovery_link;
use recovery_link::*;

fn tickets() -> [Ticket; 3] {
    [Ticket(8001), Ticket(8002), Ticket(8003)]
}
async fn complete(link: &mut Link, cx: &Cx, host: &mut Replacement, viewer: &mut Replacement) {
    let original = (host.deadline_micros(), viewer.deadline_micros());
    loop {
        assert!(net::clock(cx) < original.0);
        let h = host.advance(cx, &mut link.h, || true).unwrap();
        let c = viewer.advance(cx, &mut link.c, || true).unwrap();
        assert_eq!((host.deadline_micros(), viewer.deadline_micros()), original);
        if h && c {
            break;
        }
        link.drive(cx).await;
    }
}
async fn replace(
    link: &mut Link,
    cx: &Cx,
    host: NegotiatedMedia,
    viewer: NegotiatedMedia,
) -> (NegotiatedMedia, NegotiatedMedia) {
    let until = net::clock(cx) + 1_500_000;
    let mut h = host
        .begin_replacement(
            cx,
            &mut link.h,
            link.hr,
            parent(),
            until,
            Some(tickets()),
            || true,
        )
        .unwrap();
    let mut c = viewer
        .begin_replacement(cx, &mut link.c, link.cr, parent(), until, None, || true)
        .unwrap();
    complete(link, cx, &mut h, &mut c).await;
    (
        h.finish(cx, &mut link.h).unwrap(),
        c.finish(cx, &mut link.c).unwrap(),
    )
}
fn observation(cx: &Cx) -> frd::media::ObservationControl {
    use fr_core::authority::{AuthorityPolicy, SessionAuthority};
    let now = frd::media::host_now(cx).unwrap();
    let mut authority =
        SessionAuthority::new(parent().remote_session, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(now).unwrap();
    frd::media::ObservationControl::new(cx.clone(), authority).unwrap()
}

use fr_media::{
    delivery::SendPolicy,
    worker::{Configuration, Role as WorkerRole},
};
use frd::{
    media::{CaptureSource, ObservationControl},
    media_quic::{Error, QuicEgress},
    worker::{Deadline, Launch},
};

async fn source(control: &ObservationControl) -> CaptureSource {
    let path = worker_support::image(
        "fr-handoff",
        include_str!("support/recovery_worker_fixture.py").replace("@MODE@", "healthy"),
    );
    worker_support::capture(control, &path, 1).await
}
fn request(mut view: Binding) -> [u8; wire::REQUEST_BYTES] {
    view.parent = parent();
    let mut bytes = [0; wire::REQUEST_BYTES];
    wire::encode(
        wire::Request {
            reason: wire::Reason::ReferenceExpired,
            last_useful_frame: None,
        },
        view,
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        fr_wire::input::InputDirection::ViewerToHost,
        fr_wire::input::InputDelivery::Reliable,
    )
    .unwrap();
    bytes
}
async fn report(
    link: &mut Link,
    cx: &Cx,
    media: &NegotiatedMedia,
    sender: &mut QuicEgress,
    source: &mut CaptureSource,
) {
    let bytes = request(media.binding());
    link.c
        .send(
            cx,
            Route::Stream(link.cr.outbound),
            &bytes,
            net::clock(cx) + 1_000_000,
            || true,
        )
        .unwrap();
    let mut seen = false;
    while !seen {
        link.drive(cx).await;
        let mut record = None;
        link.h
            .receive_ready(
                cx,
                || true,
                |r| r == Route::Stream(link.hr.inbound),
                |r, b| {
                    record = Some((r, b.to_vec()));
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if let Some((r, b)) = record {
            assert!(
                media
                    .request_recovery(&link.h, link.hr, parent(), sender, source, r, &b)
                    .unwrap()
            );
            seen = true;
        }
    }
}
async fn stop(source: &mut CaptureSource, cx: &Cx) {
    let deadline = Deadline::after(cx, Duration::from_secs(1)).unwrap();
    let worker = source.worker_mut();
    worker
        .request(cx, fr_media::worker::Kind::Stop, vec![], deadline)
        .await
        .unwrap();
    worker.reap(cx, deadline).await.unwrap();
}
#[test]
fn real_report_fresh_channels_and_same_sender_preserve_chronic_failure_credit() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (hm, cm, _receiver) = link.media(&cx).await;
        let control = observation(&cx);
        let mut source = source(&control).await;
        let worker = source.worker_id();
        let mut sender = hm
            .sender(
                &link.h,
                control.clone(),
                SendPolicy {
                    max_recoveries_per_window: 1,
                    ..SendPolicy::default()
                },
            )
            .unwrap();
        sender
            .enqueue_capture(source.capture_if_changed(&control, true).await.unwrap())
            .unwrap();
        report(&mut link, &cx, &hm, &mut sender, &mut source).await;
        assert_eq!(sender.cache_usage().bytes, 0);
        assert!(!sender.is_closed());
        assert!(sender.pending().is_none());
        let fresh = source.capture_if_changed(&control, false).await.unwrap();
        assert!(fresh.encoded().unwrap().is_idr());
        let (hm, _cm) = replace(&mut link, &cx, hm, cm).await;
        hm.recover_sender(&link.h, &mut sender).unwrap();
        sender.enqueue_capture(fresh).unwrap();
        assert_eq!(source.worker_id(), worker);
        assert!(matches!(
            hm.request_recovery(
                &link.h,
                link.hr,
                parent(),
                &mut sender,
                &mut source,
                Route::Stream(link.hr.inbound),
                &request(hm.binding())
            ),
            Err(Error::Media(frd::media::Error::Send(
                fr_media::delivery::SendError::RecoveryLimitExceeded
            )))
        ));
        assert_eq!(source.next_recovery_deadline(), None);
        assert_eq!(sender.cache_usage().bytes, 0);
        stop(&mut source, &cx).await;
    });
}
#[test]
fn malformed_wrong_route_and_foreign_sender_do_not_retire_healthy_work() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (hm, _, _) = link.media(&cx).await;
        let control = observation(&cx);
        let mut source = source(&control).await;
        let mut sender = hm
            .sender(&link.h, control.clone(), SendPolicy::default())
            .unwrap();
        sender
            .enqueue_capture(source.capture_if_changed(&control, true).await.unwrap())
            .unwrap();
        let charged = sender.cache_usage();
        for (route, bytes) in [
            (Route::Stream(link.hr.inbound), b"bad".as_slice()),
            (
                Route::Stream(link.hr.outbound),
                request(hm.binding()).as_slice(),
            ),
        ] {
            assert!(
                hm.request_recovery(
                    &link.h,
                    link.hr,
                    parent(),
                    &mut sender,
                    &mut source,
                    route,
                    bytes
                )
                .is_err()
            );
            assert_eq!(sender.cache_usage(), charged);
            assert_eq!(source.next_recovery_deadline(), None);
        }
        let foreign = Link::new(&cx, true).await;
        assert!(
            hm.request_recovery(
                &foreign.h,
                foreign.hr,
                parent(),
                &mut sender,
                &mut source,
                Route::Stream(foreign.hr.inbound),
                &request(hm.binding())
            )
            .is_err()
        );
        assert!(!foreign.h.is_closed());
        assert!(!link.h.is_closed());
        assert_eq!(sender.cache_usage(), charged);
        assert!(hm.recover_sender(&link.h, &mut sender).is_err());
        assert_eq!(sender.cache_usage(), charged);
        stop(&mut source, &cx).await;
    });
}
#[test]
fn replacement_cannot_renew_an_expired_senders_deadline() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (hm, cm, _) = link.media(&cx).await;
        let control = observation(&cx);
        let mut source = source(&control).await;
        let mut sender = hm
            .sender(
                &link.h,
                control.clone(),
                SendPolicy {
                    recovery_horizon_micros: 150_000,
                    ..SendPolicy::default()
                },
            )
            .unwrap();
        sender
            .enqueue_capture(source.capture_if_changed(&control, true).await.unwrap())
            .unwrap();
        report(&mut link, &cx, &hm, &mut sender, &mut source).await;
        let original = sender.next_deadline().unwrap().as_micros();
        let (hm, _) = replace(&mut link, &cx, hm, cm).await;
        while net::clock(&cx) < original {
            link.drive(&cx).await;
        }
        assert!(matches!(
            hm.recover_sender(&link.h, &mut sender),
            Err(Error::Media(frd::media::Error::Send(
                fr_media::delivery::SendError::Delivery(
                    fr_media::delivery::DeliveryError::RecoveryExpired
                )
            )))
        ));
        assert!(sender.tick().is_err());
        stop(&mut source, &cx).await;
    });
}

#[path = "recovery_handoff/decoder.rs"]
mod decoder_recovery;
