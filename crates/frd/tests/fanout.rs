//! Real independent TLS/UDP connections, packetizers and supervised child IPC.
//! Canned HEVC configuration and synthetic decode replies are NOT codec evidence.
#![cfg(target_os = "linux")]
#[path = "../../fr-transport/tests/support/mod.rs"]
#[allow(dead_code)]
mod net;
#[path = "fanout/support.rs"]
mod support;
use asupersync::cx::Cx;
use fr_core::limits::ProtocolLimits;
use fr_media::delivery::{ReceivePipeline, SendPolicy, SharedFramePool};
use fr_transport::quic::Disposition;
use frd::{
    media::{CaptureSource, ObservationControl},
    media_quic::{NegotiatedMedia, fanout::*},
};
use std::time::Duration;
use support::*;
fn pool() -> SharedFramePool {
    SharedFramePool::new(ProtocolLimits::ABSOLUTE, 8 * 1024 * 1024, 32).unwrap()
}
fn admitted(r: &Report, id: &MemberId) -> u8 {
    r.members().find(|m| &m.member == id).unwrap().admitted
}
async fn receive(l: &mut Link, cx: &Cx, m: &NegotiatedMedia, r: &mut ReceivePipeline) {
    l.drive(cx).await;
    m.receive_ready(
        cx,
        &mut l.c,
        || true,
        |ch, b| {
            r.receive(ch, b, net::clock(cx)).unwrap();
            Ok(Disposition::Consumed)
        },
    )
    .unwrap();
}
async fn publish(
    g: &mut SendSet,
    s: &mut CaptureSource,
    c: &ObservationControl,
    p: &SharedFramePool,
    force: bool,
) {
    let update = s
        .capture_if_changed(c, force)
        .await
        .unwrap()
        .share(p)
        .unwrap();
    g.publish(&update).unwrap();
}
#[test]
fn round_robin_budget_survives_backpressure_and_one_viewer_leaving() {
    let rt = net::runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let (owner, _) = gate(&rt, 1);
        let (a, ai) = gate(&rt, 2);
        let (b, bi) = gate(&rt, 3);
        let mut source = source(&owner).await;
        let worker = source.worker_id();
        let mut la = Link::new(&cx, 2).await;
        let mut lb = Link::new(&cx, 3).await;
        let (ra, ma, mut pa, mut va) =
            Box::pin(la.ready(&cx, &mut source, &owner, &a, SendPolicy::default())).await;
        let (rb, mb, mut pb, mut vb) =
            Box::pin(lb.ready(&cx, &mut source, &owner, &b, SendPolicy::default())).await;
        let mut group = SendSet::new(8).unwrap();
        let ia = group.insert(&mut Some(ra)).unwrap();
        let ib = group.insert(&mut Some(rb)).unwrap();
        let pool = pool();
        publish(&mut group, &mut source, &owner, &pool, true).await;
        for expected in [&ia, &ib] {
            let r = group
                .transmit(&cx, &mut [(&ia, &mut la.h), (&ib, &mut lb.h)], 1)
                .unwrap();
            assert_eq!(r.attempts(), 1);
            assert_eq!(admitted(&r, expected), 1);
        }
        for _ in 0..10 {
            group
                .transmit(&cx, &mut [(&ia, &mut la.h), (&ib, &mut lb.h)], 8)
                .unwrap();
            receive(&mut la, &cx, &ma, &mut va).await;
            receive(&mut lb, &cx, &mb, &mut vb).await;
        }
        for v in [&mut va, &mut vb] {
            let n = net::clock(&cx);
            let p = v.take_decodable(n).unwrap().unwrap();
            v.complete_decode(&p, n).unwrap();
        }
        publish(&mut group, &mut source, &owner, &pool, false).await;
        // Do not drive A's socket: its bounded native datagram queue fills.
        let mut saw_pending = false;
        let mut b_admitted = 0;
        for _ in 0..12 {
            let r = group
                .transmit(&cx, &mut [(&ia, &mut la.h), (&ib, &mut lb.h)], 8)
                .unwrap();
            saw_pending |= r.members().find(|m| m.member == ia).unwrap().pending;
            b_admitted += usize::from(admitted(&r, &ib));
            receive(&mut lb, &cx, &mb, &mut vb).await;
        }
        assert!(saw_pending);
        assert!(b_admitted > 4);
        assert!(input_live(&cx, &ai));
        assert!(input_live(&cx, &bi));
        let n = net::clock(&cx);
        let p = vb.take_decodable(n).unwrap().unwrap();
        assert_eq!(p.bytes().len(), 9000);
        vb.complete_decode(&p, n).unwrap();
        drop(p);
        group.retire(&ia).unwrap();
        assert!(!input_live(&cx, &ai));
        assert!(input_live(&cx, &bi));
        assert!(!lb.h.is_closed());
        assert_eq!(source.worker_id(), worker);
        assert_eq!(group.active(), 1);
        publish(&mut group, &mut source, &owner, &pool, false).await;
        for _ in 0..12 {
            group.transmit(&cx, &mut [(&ib, &mut lb.h)], 8).unwrap();
            receive(&mut lb, &cx, &mb, &mut vb).await;
        }
        let n = net::clock(&cx);
        let p = vb.take_decodable(n).unwrap().unwrap();
        vb.complete_decode(&p, n).unwrap();
        drop(p);
        drop(group);
        assert!(!input_live(&cx, &bi));
        assert_eq!(pool.usage().bytes, 0);
        pa.abort();
        pb.abort();
        pa.reap(
            &cx,
            frd::worker::Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        pb.reap(
            &cx,
            frd::worker::Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        stop(&mut source, &cx).await;
    });
}
#[test]
fn mapping_preflight_and_reused_handles_cannot_redirect_original_senders() {
    let rt = net::runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let (owner, _) = gate(&rt, 1);
        let (a, ai) = gate(&rt, 2);
        let (b, _) = gate(&rt, 3);
        let mut source = source(&owner).await;
        let mut la = Link::new(&cx, 2).await;
        let mut lb = Link::new(&cx, 3).await;
        let (ra, _, mut pa, _) =
            Box::pin(la.ready(&cx, &mut source, &owner, &a, SendPolicy::default())).await;
        let (rb, _, mut pb, _) =
            Box::pin(lb.ready(&cx, &mut source, &owner, &b, SendPolicy::default())).await;
        let mut group = SendSet::new(1).unwrap();
        let ia = group.insert(&mut Some(ra)).unwrap();
        let mut rb = Some(rb);
        let e = group.insert(&mut rb).unwrap_err();
        assert_eq!(e, Error::Full);
        assert_eq!(
            group.transmit(&cx, &mut [(&ia, &mut lb.h)], 1).unwrap_err(),
            Error::WrongConnections
        );
        assert_eq!(
            group.transmit(&cx, &mut [], 1).unwrap_err(),
            Error::WrongConnections
        );
        assert_eq!(
            group.transmit(&cx, &mut [(&ia, &mut la.h)], 0).unwrap_err(),
            Error::InvalidPolicy
        );
        assert!(input_live(&cx, &ai));
        assert!(!la.h.is_closed());
        assert!(!lb.h.is_closed());
        let old = group.detach(&ia).unwrap();
        let ib = group.insert(&mut rb).unwrap();
        assert_eq!(group.detach(&ia).unwrap_err(), Error::StaleMember);
        let mut other = SendSet::new(1).unwrap();
        let other_id = other.insert(&mut Some(old)).unwrap();
        assert_eq!(group.detach(&other_id).unwrap_err(), Error::StaleMember);
        assert_eq!(other.detach(&ib).unwrap_err(), Error::StaleMember);
        drop(group);
        drop(other);
        assert!(!input_live(&cx, &ai));
        pa.abort();
        pb.abort();
        pa.reap(
            &cx,
            frd::worker::Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        pb.reap(
            &cx,
            frd::worker::Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        stop(&mut source, &cx).await;
    });
}

#[test]
fn idle_expiry_releases_only_the_failed_members_storage_and_input_evidence() {
    let rt = net::runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let (owner, _) = gate(&rt, 1);
        let (a, ai) = gate(&rt, 2);
        let (b, bi) = gate(&rt, 3);
        let mut source = source(&owner).await;
        let mut la = Link::new(&cx, 2).await;
        let mut lb = Link::new(&cx, 3).await;
        let (ra, _, mut pa, _) = Box::pin(la.ready(
            &cx,
            &mut source,
            &owner,
            &a,
            SendPolicy {
                reference_horizon_micros: 10_000,
                ..SendPolicy::default()
            },
        ))
        .await;
        let (rb, _, mut pb, _) =
            Box::pin(lb.ready(&cx, &mut source, &owner, &b, SendPolicy::default())).await;
        let mut group = SendSet::new(8).unwrap();
        let ia = group.insert(&mut Some(ra)).unwrap();
        let ib = group.insert(&mut Some(rb)).unwrap();
        let pool = pool();
        publish(&mut group, &mut source, &owner, &pool, true).await;
        let charged = pool.usage();
        assert_eq!(charged.pictures, 1);
        asupersync::time::sleep(cx.timer_driver().unwrap().now(), Duration::from_millis(20)).await;
        let report = group.tick();
        let failure = report.members().find(|m| m.member == ia).unwrap().failure;
        assert!(failure.is_some());
        assert!(
            report
                .members()
                .find(|m| m.member == ib)
                .unwrap()
                .failure
                .is_none()
        );
        assert_eq!(group.active(), 1);
        assert_eq!(group.len(), 2);
        assert!(!input_live(&cx, &ai));
        assert!(input_live(&cx, &bi));
        assert_eq!(pool.usage(), charged);
        assert!(group.next_deadline().is_some());
        assert_eq!(
            group
                .tick()
                .members()
                .find(|m| m.member == ia)
                .unwrap()
                .failure,
            failure
        );
        group.retire(&ia).unwrap();
        assert_eq!(pool.usage(), charged);
        group.retire(&ib).unwrap();
        assert_eq!(pool.usage().bytes, 0);
        assert!(!input_live(&cx, &bi));
        assert!(!la.h.is_closed());
        assert!(!lb.h.is_closed());
        pa.abort();
        pb.abort();
        pa.reap(
            &cx,
            frd::worker::Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        pb.reap(
            &cx,
            frd::worker::Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        stop(&mut source, &cx).await;
    });
}

#[test]
fn a_foreign_source_cannot_join_or_publish_even_after_the_last_member_leaves() {
    let rt = net::runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let (owner, _) = gate(&rt, 1);
        let (a, ai) = gate(&rt, 2);
        let (b, bi) = gate(&rt, 3);
        let mut first = source(&owner).await;
        let mut foreign = source(&owner).await;
        let mut la = Link::new(&cx, 2).await;
        let mut lb = Link::new(&cx, 3).await;
        let (ra, _, mut pa, _) =
            Box::pin(la.ready(&cx, &mut first, &owner, &a, SendPolicy::default())).await;
        let (rb, _, mut pb, _) =
            Box::pin(lb.ready(&cx, &mut foreign, &owner, &b, SendPolicy::default())).await;
        let mut group = SendSet::new(8).unwrap();
        let ia = group.insert(&mut Some(ra)).unwrap();
        let mut rb = Some(rb);
        let e = group.insert(&mut rb).unwrap_err();
        assert_eq!(e, Error::WrongSource);
        assert!(input_live(&cx, &ai));
        assert!(input_live(&cx, &bi));
        let update = foreign
            .capture_if_changed(&owner, true)
            .await
            .unwrap()
            .share(&pool())
            .unwrap();
        assert_eq!(group.publish(&update).unwrap_err(), Error::WrongSource);
        assert_eq!(group.active(), 1);
        assert!(input_live(&cx, &ai));
        let original = group.detach(&ia).unwrap();
        assert!(group.is_empty());
        let e = group.insert(&mut rb).unwrap_err();
        assert_eq!(e, Error::WrongSource);
        drop(rb);
        assert!(!input_live(&cx, &bi));
        let replacement = group.insert(&mut Some(original)).unwrap();
        assert_ne!(replacement, ia);
        group.retire(&replacement).unwrap();
        assert!(!input_live(&cx, &ai));
        pa.abort();
        pb.abort();
        pa.reap(
            &cx,
            frd::worker::Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        pb.reap(
            &cx,
            frd::worker::Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        stop(&mut first, &cx).await;
        stop(&mut foreign, &cx).await;
    });
}

#[test]
fn closed_connection_is_reported_without_stopping_another_members_turn() {
    let rt = net::runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let (owner, _) = gate(&rt, 1);
        let (a, ai) = gate(&rt, 2);
        let (b, bi) = gate(&rt, 3);
        let mut source = source(&owner).await;
        let mut la = Link::new(&cx, 2).await;
        let mut lb = Link::new(&cx, 3).await;
        let (ra, _, mut pa, _) =
            Box::pin(la.ready(&cx, &mut source, &owner, &a, SendPolicy::default())).await;
        let (rb, _, mut pb, _) =
            Box::pin(lb.ready(&cx, &mut source, &owner, &b, SendPolicy::default())).await;
        let mut group = SendSet::new(8).unwrap();
        let ia = group.insert(&mut Some(ra)).unwrap();
        let ib = group.insert(&mut Some(rb)).unwrap();
        publish(&mut group, &mut source, &owner, &pool(), true).await;
        la.h.close();
        let result = group
            .transmit(&cx, &mut [(&ia, &mut la.h), (&ib, &mut lb.h)], 2)
            .unwrap();
        assert!(
            result
                .members()
                .find(|m| m.member == ia)
                .unwrap()
                .failure
                .is_some()
        );
        assert_eq!(admitted(&result, &ib), 1);
        assert!(!input_live(&cx, &ai));
        assert!(input_live(&cx, &bi));
        assert!(!lb.h.is_closed());
        owner.check().unwrap();
        assert_eq!(group.active(), 1);
        group.retire(&ia).unwrap();
        let current = group.detach(&ib).unwrap().into_sender();
        assert!(!current.is_closed());
        assert!(input_live(&cx, &bi));
        let mut current = current;
        current.close();
        assert!(!input_live(&cx, &bi));
        pa.abort();
        pb.abort();
        pa.reap(
            &cx,
            frd::worker::Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        pb.reap(
            &cx,
            frd::worker::Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        stop(&mut source, &cx).await;
    });
}

#[test]
fn cancellation_and_closed_members_cannot_replenish_send_ownership() {
    assert!(matches!(SendSet::new(0), Err(Error::InvalidPolicy)));
    assert!(matches!(SendSet::new(9), Err(Error::InvalidPolicy)));
    let rt = net::runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let (owner, _) = gate(&rt, 1);
        let (a, ai) = gate(&rt, 2);
        let mut source = source(&owner).await;
        let mut link = Link::new(&cx, 2).await;
        let (ready, _, mut p, _) =
            Box::pin(link.ready(&cx, &mut source, &owner, &a, SendPolicy::default())).await;
        let mut group = SendSet::new(8).unwrap();
        assert_eq!(group.insert(&mut None).unwrap_err(), Error::MissingSender);
        let id = group.insert(&mut Some(ready)).unwrap();
        publish(&mut group, &mut source, &owner, &pool(), true).await;
        let cancelled = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
        cancelled.cancel_fast(asupersync::types::CancelKind::User);
        assert_eq!(
            group
                .transmit(&cancelled, &mut [(&id, &mut link.h)], 1)
                .unwrap_err(),
            Error::Closed
        );
        assert!(!input_live(&cx, &ai));
        assert!(!link.h.is_closed());
        owner.check().unwrap();
        assert_eq!(group.next_deadline(), None);
        let mut failed = Some(group.detach(&id).unwrap());
        let mut other = SendSet::new(8).unwrap();
        assert_eq!(other.insert(&mut failed).unwrap_err(), Error::Closed);
        assert!(failed.is_some());
        p.abort();
        p.reap(
            &cx,
            frd::worker::Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        stop(&mut source, &cx).await;
    });
}
