//! Real sender expiry between the two production receive phases. No clock or
//! source progress is injected; the original subscriber discovers its own loss.
use super::*;
use fr_wire::presented as report_wire;

type ReceiveHandler = fn(Route, &[u8]) -> Result<Disposition, ()>;

fn with_services<T>(
    peer: &mut Member,
    run: impl FnOnce(&mut SharedServices<'_, ReceiveHandler>, &QuicRecords) -> T,
) -> T {
    let host = peer.shared.as_mut().unwrap();
    let mut other: ReceiveHandler = block;
    let mut services = SharedServices {
        subscriber: host.subscriber.as_mut().unwrap(),
        control: host.session.opened.control.clone(),
        repair: &mut host.repair,
        routes: host.session.opened.routes,
        parent: host.session.opened.binding,
        selection: &host.selection,
        recovery: &mut host.recovery,
        presentation: &mut host.presentation,
        feedback: &mut host.feedback,
        statistics: &mut host.statistics,
        other: &mut other,
    };
    run(&mut services, &host.session.opened.transport)
}

fn view(peer: &Member) -> decoder::Binding {
    let mut view = peer.media.binding();
    view.parent = peer.shared.as_ref().unwrap().session.binding();
    view
}

fn encode_report(report: report_wire::Report, view: decoder::Binding) -> Vec<u8> {
    let mut bytes = vec![0; report_wire::BYTES];
    report_wire::encode(
        report,
        view,
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes
}

fn foreign_reports(bytes: &[u8], view: decoder::Binding) -> [Vec<u8>; 2] {
    let report = report_wire::decode(
        bytes,
        view,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    let mut foreign_session = view;
    foreign_session.parent.remote_session = RemoteSessionId::from_raw(999);
    let mut foreign_recovery = view;
    foreign_recovery.recovery = view.recovery.next().unwrap();
    [
        encode_report(report, foreign_session),
        encode_report(report, foreign_recovery),
    ]
}

fn check_obsolete_reports(
    services: &mut SharedServices<'_, ReceiveHandler>,
    q: &QuicRecords,
    bytes: &[u8],
    foreign: &[Vec<u8>; 2],
    until: u64,
) {
    let route = Route::Stream(services.routes.inbound);
    assert_eq!(services.receive(route, bytes), Ok(Disposition::Consumed));
    assert_eq!(services.statistics.presentation_reports, 0);
    // Direct handler calls inspect the refusal; the transport is responsible
    // for terminating a peer when its actual receive callback returns Err.
    assert_eq!(
        services.receive(Route::Stream(services.routes.outbound), bytes),
        Err(())
    );
    assert_eq!(services.receive(*services.repair, bytes), Err(()));
    let mut trailing = bytes.to_vec();
    trailing.push(0);
    for invalid in [
        &bytes[..bytes.len() - 1],
        trailing.as_slice(),
        foreign[0].as_slice(),
        foreign[1].as_slice(),
    ] {
        assert_eq!(services.receive(route, invalid), Err(()));
    }
    assert_eq!(services.subscriber.recovery_deadline(q), Ok(Some(until)));
    assert_eq!(services.statistics.presentation_reports, 0);
}

#[test]
fn source_expiry_during_presentation_dispatch_preserves_original_bounded_recovery() {
    let rt = support::runtime();
    let cleanup_cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let policy = SendPolicy {
            reference_horizon_micros: 40_000,
            ..SendPolicy::default()
        };
        let mut g = Box::pin(group_with_policy(
            &rt,
            1,
            &[wire::CAPABILITY, report_wire::CAPABILITY],
            true,
            policy,
        ))
        .await;
        ready(&mut g).await;
        let worker = g.publisher.worker_id();
        let original = view(&g.peers[0]);
        let bytes = presentation(&g.peers[0]);
        let foreign = foreign_reports(&bytes, original);
        let unsent = g.publisher.capture_next().await.unwrap();
        assert_eq!((unsent.delivered, unsent.refused), (1, 0));
        let until = with_services(&mut g.peers[0], |services, q| {
            let route = Route::Stream(services.routes.inbound);
            // This is receive's first phase. The reference is still valid and
            // the existing recovery dispatcher has nothing to consume.
            assert_eq!(services.recovery_record(route, &bytes), Ok(None));
            assert_eq!(services.subscriber.locally_failed_view(), Ok(None));
            std::thread::sleep(Duration::from_millis(60));
            // Invoke receive's actual next phase. Its session_progress call
            // must discover expiry itself, after the earlier preflight passed.
            assert_eq!(
                services.presentation_record(route, &bytes),
                Ok(Disposition::Consumed)
            );
            assert!(services.subscriber.locally_failed_view().unwrap().is_some());
            assert_eq!(services.subscriber.session_progress(), Ok(None));
            let until = services.subscriber.recovery_deadline(q).unwrap().unwrap();
            assert_eq!(services.presentation.as_ref().unwrap().accepted, 0);
            assert!(services.control.check().is_ok());
            assert!(!q.is_closed());
            check_obsolete_reports(services, q, &bytes, &foreign, until);
            until
        });
        assert!(g.owner.check().is_ok());
        // The ordinary authenticated request must inherit the SAME local
        // deadline. During handoff its old reports still require full parsing.
        admit(&mut g.peers[0]).await;
        assert_eq!(deadline(&mut g.peers[0]), until);
        with_services(&mut g.peers[0], |services, q| {
            assert!(services.recovery.is_some());
            assert!(services.presentation.is_none());
            check_obsolete_reports(services, q, &bytes, &foreign, until);
        });
        let failed = g.peers.remove(0);
        let recovered = Box::pin(replace(failed)).await;
        g.peers.push(recovered);
        let idr = g.publisher.capture_next().await.unwrap();
        ready(&mut g).await;
        let peer = &g.peers[0];
        assert_eq!(
            peer.media.binding().recovery,
            original.recovery.next().unwrap()
        );
        assert!(peer.frames.contains(&idr.frame));
        assert_eq!(
            peer.shared.as_ref().unwrap().statistics().recovered_streams,
            1
        );
        assert_eq!(g.publisher.worker_id(), worker);
        cleanup(&mut g, &cleanup_cx).await;
    });
}

#[test]
fn missing_startup_progress_is_not_treated_as_local_recovery() {
    let rt = support::runtime();
    let cleanup_cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group_with_capabilities(
            &rt,
            1,
            &[wire::CAPABILITY, report_wire::CAPABILITY],
            false,
        ))
        .await;
        let bytes = encode_report(
            report_wire::Report {
                sequence: 1,
                visible: None,
            },
            view(&g.peers[0]),
        );
        with_services(&mut g.peers[0], |services, q| {
            assert!(services.presentation.is_some());
            assert_eq!(services.subscriber.session_progress(), Ok(None));
            assert_eq!(services.subscriber.locally_failed_view(), Ok(None));
            assert_eq!(
                services.receive(Route::Stream(services.routes.inbound), &bytes),
                Err(())
            );
            assert_eq!(services.subscriber.recovery_deadline(q), Ok(None));
            assert_eq!(services.statistics.presentation_reports, 0);
        });
        cleanup(&mut g, &cleanup_cx).await;
    });
}

#[test]
fn recovery_does_not_admit_an_unselected_presentation_capability() {
    let rt = support::runtime();
    let cleanup_cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let policy = SendPolicy {
            reference_horizon_micros: 40_000,
            ..SendPolicy::default()
        };
        let mut g = Box::pin(group_with_policy(&rt, 1, &[wire::CAPABILITY], true, policy)).await;
        ready(&mut g).await;
        let bytes = encode_report(
            report_wire::Report {
                sequence: 1,
                visible: None,
            },
            view(&g.peers[0]),
        );
        g.publisher.capture_next().await.unwrap();
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(g.publisher.tick(), Ok(1));
        let until = deadline(&mut g.peers[0]);
        with_services(&mut g.peers[0], |services, q| {
            assert!(services.presentation.is_none());
            assert!(services.subscriber.locally_failed_view().unwrap().is_some());
            assert_eq!(
                services.receive(Route::Stream(services.routes.inbound), &bytes),
                Err(())
            );
            assert_eq!(services.subscriber.recovery_deadline(q), Ok(Some(until)));
        });
        admit(&mut g.peers[0]).await;
        with_services(&mut g.peers[0], |services, q| {
            assert!(services.recovery.is_some());
            assert_eq!(
                services.receive(Route::Stream(services.routes.inbound), &bytes),
                Err(())
            );
            assert_eq!(services.subscriber.recovery_deadline(q), Ok(Some(until)));
        });
        cleanup(&mut g, &cleanup_cx).await;
    });
}
