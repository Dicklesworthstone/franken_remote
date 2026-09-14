//! Real authenticated session transport; source and platform visibility are
//! explicit fixtures. These assertions qualify authority composition, not HEVC.
use super::*;
use crate::session_startup::{
    ViewerSession,
    running::tests::{pair_initialized, run},
    tests::support,
};
use fr_core::{ids::*, limits::ProtocolLimits, time::HostInstant};
use fr_wire::{
    FrameDescriptor, PipelineState, SourceObservation,
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    negotiation::Capability,
    presented::{self as wire, Report, Sample, Stamp},
};

fn view(parent: fr_wire::negotiation::ControlBinding) -> Binding {
    Binding {
        parent,
        display: 8,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
fn sample(at: u64) -> Sample {
    Sample {
        stamp: Stamp {
            frame: 0,
            captured_us: at,
            observed_us: at,
            source: SourceObservation::Captured,
        },
        age_upper_us: 0,
    }
}
fn progress(sample: Sample) -> fr_wire::Progress {
    fr_wire::Progress {
        descriptor: FrameDescriptor {
            frame: sample.stamp.frame,
            reference: None,
            capture_micros: sample.stamp.captured_us,
            total_bytes: 4,
            stride: 4,
        },
        observed_micros: sample.stamp.observed_us,
        observation: sample.stamp.source,
        pipeline: PipelineState::Running,
    }
}
fn bytes(binding: Binding, sequence: u64, visible: Option<Sample>) -> Vec<u8> {
    let mut out = vec![0; wire::BYTES];
    wire::encode(
        Report { sequence, visible },
        binding,
        &ProtocolLimits::ABSOLUTE,
        &mut out,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    out
}
async fn fixture(c: &Cx, h: &Cx) -> (HostSession, ViewerSession, HostPresentation) {
    let (mut host, viewer) = pair_initialized(
        c,
        h,
        vec![Capability {
            name: wire::CAPABILITY.into(),
            version: wire::VERSION,
            required: true,
        }],
        |host| {
            host.authority
                .as_mut()
                .unwrap()
                .mark_view_ready(HostInstant::from_micros(now(h).unwrap()))
                .unwrap();
        },
    )
    .await;
    let parent = host.binding();
    let selection = host.selection().clone();
    let control = host.observation().unwrap();
    assert!(control.view_ready().unwrap());
    let (q, routes) = host.io().unwrap();
    let proof = HostPresentation::attach(
        &selection,
        parent,
        view(parent),
        q,
        routes.inbound,
        control.clone(),
    )
    .unwrap()
    .unwrap();
    assert!(!control.view_ready().unwrap());
    (host, viewer, proof)
}
#[allow(clippy::unnecessary_wraps)]
fn block(_: Route, _: &[u8]) -> Result<Disposition, ()> {
    Ok(Disposition::Blocked)
}

#[test]
fn actual_session_report_opens_only_readiness_and_expires_without_more_network() {
    run(|c, h| async move {
        let (mut host, mut viewer, mut proof) = fixture(&c, &h).await;
        let control = host.observation().unwrap();
        let s = sample(now(&h).unwrap());
        proof.observe(Some(progress(s))).unwrap();
        let record = bytes(view(host.binding()), 1, Some(s));
        let mut sent = false;
        let mut nonce = 2000;
        let until = s.stamp.observed_us + wire::MAX_SOURCE_AGE_US;
        while proof.accepted == 0 {
            assert!(now(&h).unwrap() < until);
            if !sent {
                let (q, routes) = viewer.io().unwrap();
                match q.send(&c, Route::Stream(routes.outbound), &record, until, || true) {
                    Ok(()) => sent = true,
                    Err(fr_transport::quic::Error::Backpressure) => {}
                    Err(e) => panic!("report admission: {e:?}"),
                }
            }
            let (a, b) = Box::pin(support::both(
                host.drive(
                    Duration::from_millis(1),
                    || {
                        nonce += 1;
                        Ok(nonce)
                    },
                    |route, data| {
                        proof.receive(route, data).unwrap();
                        Ok(Disposition::Consumed)
                    },
                ),
                viewer.drive(Duration::from_millis(1), block),
            ))
            .await;
            a.unwrap();
            b.unwrap();
        }
        assert!(control.view_ready().unwrap());
        // A report has not minted a lease, ticket, or native input owner.
        assert!(control.check_control().is_err());
        asupersync::time::sleep(
            h.now(),
            Duration::from_micros(until - now(&h).unwrap() + 1_000),
        )
        .await;
        assert!(!control.view_ready().unwrap());
        assert!(control.check().is_ok());
        host.close();
        viewer.close();
    });
}
#[test]
fn unavailable_and_same_source_replay_cannot_restore_host_readiness() {
    run(|c, h| async move {
        let (mut host, mut viewer, mut proof) = fixture(&c, &h).await;
        let control = host.observation().unwrap();
        let binding = view(host.binding());
        let route = Route::Stream(host.io().unwrap().1.inbound);
        let s = sample(now(&h).unwrap());
        proof.observe(Some(progress(s))).unwrap();
        proof.receive(route, &bytes(binding, 1, Some(s))).unwrap();
        assert!(control.view_ready().unwrap());
        proof.receive(route, &bytes(binding, 2, None)).unwrap();
        assert!(!control.view_ready().unwrap());
        proof.receive(route, &bytes(binding, 3, Some(s))).unwrap();
        assert!(!control.view_ready().unwrap());
        assert_eq!(proof.accepted, 1);
        assert!(proof.receive(route, &bytes(binding, 3, Some(s))).is_err());
        host.close();
        viewer.close();
    });
}
#[test]
fn unknown_source_foreign_view_and_wrong_route_suspend_before_dispatch_returns() {
    run(|c, h| async move {
        let (mut host, mut viewer, mut proof) = fixture(&c, &h).await;
        let control = host.observation().unwrap();
        let binding = view(host.binding());
        let routes = host.io().unwrap().1;
        let s = sample(now(&h).unwrap());
        proof.observe(Some(progress(s))).unwrap();
        let incoming = Route::Stream(routes.inbound);
        proof
            .receive(incoming, &bytes(binding, 1, Some(s)))
            .unwrap();
        let mut unknown = progress(s);
        unknown.observation = SourceObservation::Unknown;
        unknown.observed_micros = 0;
        proof.observe(Some(unknown)).unwrap();
        assert!(!control.view_ready().unwrap());
        let mut unissued = s;
        unissued.stamp.observed_us += 1;
        unissued.stamp.source = SourceObservation::QualifiedUnchanged;
        assert!(
            proof
                .receive(incoming, &bytes(binding, 2, Some(unissued)))
                .is_err()
        );
        let mut foreign = binding;
        foreign.display += 1;
        assert!(
            proof
                .receive(incoming, &bytes(foreign, 3, Some(s)))
                .is_err()
        );
        assert!(
            proof
                .receive(Route::Stream(routes.outbound), &bytes(binding, 4, Some(s)))
                .is_err()
        );
        assert!(!control.view_ready().unwrap());
        host.close();
        viewer.close();
    });
}
#[test]
fn proof_attachment_and_drop_never_allow_connection_or_authority_replacement() {
    run(|c, h| async move {
        let (mut host, mut viewer, mut proof) = fixture(&c, &h).await;
        let control = host.observation().unwrap();
        let parent = host.binding();
        let selection = host.selection().clone();
        let s = sample(now(&h).unwrap());
        let (q, routes) = host.io().unwrap();
        proof.observe(Some(progress(s))).unwrap();
        proof
            .receive(
                Route::Stream(routes.inbound),
                &bytes(view(parent), 1, Some(s)),
            )
            .unwrap();
        assert!(control.view_ready().unwrap());
        let mut wrong = view(parent);
        wrong.parent.remote_session = RemoteSessionId::from_raw(999);
        assert!(
            HostPresentation::attach(
                &selection,
                parent,
                wrong,
                q,
                routes.inbound,
                control.clone()
            )
            .is_err()
        );
        assert!(proof.check_connection(viewer.io().unwrap().0).is_err());
        assert!(!viewer.io().unwrap().0.is_closed());
        assert!(!control.view_ready().unwrap());
        drop(proof);
        assert!(!control.view_ready().unwrap());
        host.close();
        viewer.close();
    });
}

#[test]
fn viewer_reports_refuse_foreign_connection_and_unselected_extension() {
    use crate::media::presented::{ViewSample, ViewerPresentation};
    run(|c, h| async move {
        let (mut host, mut viewer, _proof) = fixture(&c, &h).await;
        let parent = host.binding();
        let mut selection = host.selection().clone();
        let (q, routes) = viewer.io().unwrap();
        let mut reporter = ViewerPresentation::attach(
            &selection,
            parent,
            view(parent),
            q,
            routes.outbound,
            now(&c).unwrap(),
        )
        .unwrap()
        .unwrap();
        selection.capabilities.clear();
        assert!(
            ViewerPresentation::attach(
                &selection,
                parent,
                view(parent),
                q,
                routes.outbound,
                now(&c).unwrap()
            )
            .unwrap()
            .is_none()
        );
        let q = host.io().unwrap().0;
        let before = q.usage();
        assert_eq!(
            reporter.service(q, &c, ViewSample::Pending, now(&c).unwrap()),
            Err(presented::Error::Binding)
        );
        assert_eq!(q.usage(), before);
        assert!(!q.is_closed());
        assert_eq!(reporter.sent, 0);
        host.close();
        viewer.close();
    });
}
#[test]
fn viewer_report_deadline_includes_time_between_view_sampling_and_service() {
    use crate::media::presented::{ViewSample, ViewerPresentation};
    run(|c, h| async move {
        let (mut host, mut viewer, _proof) = fixture(&c, &h).await;
        let parent = host.binding();
        let selection = host.selection().clone();
        let (q, routes) = viewer.io().unwrap();
        let sampled_at = now(&c).unwrap();
        let mut reporter = ViewerPresentation::attach(
            &selection,
            parent,
            view(parent),
            q,
            routes.outbound,
            sampled_at,
        )
        .unwrap()
        .unwrap();
        let mut evidence = sample(sampled_at);
        evidence.age_upper_us = wire::MAX_SOURCE_AGE_US - 10_000;
        asupersync::time::sleep(c.now(), Duration::from_millis(20)).await;
        let before = q.usage();
        assert_eq!(
            reporter.service(
                q,
                &c,
                ViewSample::Visible(evidence, sampled_at),
                now(&c).unwrap()
            ),
            Err(presented::Error::Proof(fr_media::presented::Error::Expired))
        );
        assert_eq!(reporter.sent, 0);
        assert_eq!(q.usage(), before);
        host.close();
        viewer.close();
    });
}
#[test]
fn actual_critical_backpressure_does_not_extend_a_pending_presentation_deadline() {
    use crate::media::presented::{ViewSample, ViewerPresentation};
    run(|c, h| async move {
        let (mut host, mut viewer, _proof) = fixture(&c, &h).await;
        let parent = host.binding();
        let selection = host.selection().clone();
        let (q, routes) = viewer.io().unwrap();
        let start = now(&c).unwrap();
        let mut reporter =
            ViewerPresentation::attach(&selection, parent, view(parent), q, routes.outbound, start)
                .unwrap()
                .unwrap();
        let filler = bytes(view(parent), 1, None);
        loop {
            match q.send(
                &c,
                Route::Stream(routes.outbound),
                &filler,
                start + 1_000_000,
                || true,
            ) {
                Ok(()) => {}
                Err(fr_transport::quic::Error::Backpressure) => break,
                Err(e) => panic!("unexpected queue admission: {e:?}"),
            }
        }
        let mut evidence = sample(start);
        evidence.age_upper_us = wire::MAX_SOURCE_AGE_US - 100_000;
        reporter
            .service(
                q,
                &c,
                ViewSample::Visible(evidence, start),
                now(&c).unwrap(),
            )
            .unwrap();
        assert_eq!(reporter.sent, 0);
        let full = q.usage();
        asupersync::time::sleep(c.now(), Duration::from_millis(110)).await;
        let at = now(&c).unwrap();
        // Even a newer sample claiming more lifetime for the same source must
        // not reencode the already pending report or start another deadline.
        evidence.age_upper_us = 0;
        assert_eq!(
            reporter.service(q, &c, ViewSample::Visible(evidence, at), at),
            Err(presented::Error::Proof(fr_media::presented::Error::Expired))
        );
        assert_eq!(reporter.sent, 0);
        assert_eq!(q.usage(), full);
        host.close();
        viewer.close();
    });
}
