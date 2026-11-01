//! Real shared-session dispatch over UDP/TLS; reports and source pixels are
//! explicit protocol fixtures. Recovery may retire an old report, never bypass
//! the selected capability, direction, limits or original failed view.
use super::*;
use fr_wire::receiver_metrics::{self as metrics, Load, Message};

#[derive(Debug, Clone, Copy)]
enum Report {
    Valid,
    Unnegotiated,
    ForeignSession,
    FutureRecovery,
    WrongDirection,
    MalformedLoad,
}

fn report(p: &Member, kind: Report) -> Vec<u8> {
    let mut view = p.media.binding();
    view.parent = p.shared.as_ref().unwrap().session.binding();
    match kind {
        Report::ForeignSession => view.parent.remote_session = RemoteSessionId::from_raw(999),
        Report::FutureRecovery => view.recovery = view.recovery.next().unwrap(),
        _ => {}
    }
    let (message, direction) = if matches!(kind, Report::WrongDirection) {
        (Message::Query { sequence: 1 }, InputDirection::HostToViewer)
    } else {
        (
            Message::Reply {
                sequence: 1,
                load: Load {
                    retained_bytes: 0,
                    retained_pictures: 0,
                    decoding: false,
                    work_us: None,
                },
            },
            InputDirection::ViewerToHost,
        )
    };
    let mut bytes = vec![0; metrics::REPLY_BYTES];
    let length = metrics::encode(
        message,
        view,
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        direction,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes.truncate(length);
    if matches!(kind, Report::MalformedLoad) {
        // A boolean outside the wire grammar, preserving the record's kind,
        // complete envelope, session and full view.
        bytes[metrics::QUERY_BYTES + 8 + 4] = 2;
    }
    bytes
}

async fn failed_group(rt: &Runtime, kind: Report, local: bool) -> Box<Group> {
    let capabilities: &[&str] = if matches!(kind, Report::Unnegotiated) {
        &[wire::CAPABILITY]
    } else {
        &[wire::CAPABILITY, metrics::CAPABILITY]
    };
    let policy = SendPolicy {
        reference_horizon_micros: if local { 40_000 } else { 250_000 },
        ..SendPolicy::default()
    };
    let mut group = Box::pin(group_with_policy(rt, 2, capabilities, true, policy)).await;
    ready(&mut group).await;
    if local {
        let frame = group.publisher.capture_next().await.unwrap();
        // Only the first peer's original remains unadmitted. The sibling
        // consumes its actual picture before the original reference expires.
        while !group.peers[1].frames.contains(&frame.frame) {
            group.peers[1].turn().await.unwrap();
        }
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(group.publisher.tick(), Ok(2));
        assert!(
            group.peers[0]
                .shared
                .as_ref()
                .unwrap()
                .subscriber
                .as_ref()
                .unwrap()
                .awaits_recovery_request()
                .unwrap()
        );
    } else {
        admit(&mut group.peers[0]).await;
    }
    group
}

async fn retire_valid_report(rt: &Runtime, local: bool) {
    let cleanup_cx = rt.request_cx_with_budget(Budget::INFINITE);
    let mut group = Box::pin(failed_group(rt, Report::Valid, local)).await;
    let peer = &mut group.peers[0];
    let until = deadline(peer);
    let reports = peer.shared.as_ref().unwrap().statistics().feedback_reports;
    let bytes = report(peer, Report::Valid);
    queue(peer, &bytes).await;
    if local {
        // This request follows the report on the same reliable control stream.
        // The host cannot admit it while that earlier report remains parked.
        admit(peer).await;
    }
    // The replacement's control-channel acknowledgements also follow the
    // report. Reaching DecoderStartup proves the original host consumed that
    // report, rather than merely leaving it queued through a fixed poll count.
    let peer = group.peers.remove(0);
    let recovered = Box::pin(replace(peer)).await;
    group.peers.insert(0, recovered);
    let peer = &mut group.peers[0];
    assert_eq!(deadline(peer), until);
    assert_eq!(
        peer.shared.as_ref().unwrap().statistics().feedback_reports,
        reports
    );
    assert!(!peer.control.view_ready().unwrap());
    assert!(peer.control.check().is_ok());
    assert!(group.peers[1].control.check().is_ok());
    assert_eq!(group.publisher.tick(), Ok(2));
    cleanup(&mut group, &cleanup_cx).await;
}

async fn reject_invalid_reports(rt: &Runtime, local: bool) {
    let cleanup_cx = rt.request_cx_with_budget(Budget::INFINITE);
    for kind in [
        Report::Unnegotiated,
        Report::ForeignSession,
        Report::FutureRecovery,
        Report::WrongDirection,
        Report::MalformedLoad,
    ] {
        let mut group = Box::pin(failed_group(rt, kind, local)).await;
        let peer = &mut group.peers[0];
        let bytes = report(peer, kind);
        queue(peer, &bytes).await;
        let until = now(&peer.h).unwrap() + 500_000;
        while network(peer).await.is_ok() {
            assert!(
                now(&peer.h).unwrap() < until,
                "accepted {kind:?}, local={local}"
            );
        }
        assert!(peer.control.check().is_err(), "{kind:?}, local={local}");
        assert_eq!(group.publisher.tick(), Ok(1));
        assert!(group.peers[1].control.check().is_ok());
        cleanup(&mut group, &cleanup_cx).await;
    }
}

#[test]
fn obsolete_metrics_during_shared_attachment_are_retired_without_evidence_or_extra_time() {
    let rt = support::runtime();
    rt.block_on(Box::pin(retire_valid_report(&rt, false)));
}

#[test]
fn obsolete_metrics_after_local_reference_failure_are_retired_without_evidence_or_extra_time() {
    let rt = support::runtime();
    rt.block_on(Box::pin(retire_valid_report(&rt, true)));
}

#[test]
fn invalid_metrics_during_shared_attachment_close_only_the_sending_peer() {
    let rt = support::runtime();
    rt.block_on(Box::pin(reject_invalid_reports(&rt, false)));
}

#[test]
fn invalid_metrics_after_local_reference_failure_close_only_the_sending_peer() {
    let rt = support::runtime();
    rt.block_on(Box::pin(reject_invalid_reports(&rt, true)));
}
