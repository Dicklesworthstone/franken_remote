//! Production session and QUIC owners, real supervised IPC, synthetic codec.
use super::*;
use fr_media::pacing::{Mode, Reason};

/// The actual host/viewer offers enter the real startup codecs independently.
/// Identity and the later capture/decode fixture remain explicitly synthetic.
async fn reporting_sessions(c: &Cx, h: &Cx) -> (HostSession, ViewerSession) {
    use crate::session_startup::{Configuration, Peer};
    use fr_core::authority::AuthorityPolicy;
    use fr_transport::quic::{ALPN, Policy as TransportPolicy};
    use fr_wire::{negotiation::ControlBinding, quality_decision};

    let offer =
        crate::session_startup::with_quality_decisions(crate::session_startup::host_offer(false))
            .unwrap();
    let cfg = Configuration {
        offer,
        binding: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(13),
        },
        require_approval: false,
        startup_timeout: Duration::from_secs(2),
        authority: AuthorityPolicy::plan_defaults(),
        transport: TransportPolicy::default(),
    };
    let (client, native) = support::native_pair(c, "localhost", ALPN).await;
    let mut viewer = crate::session_startup::Viewer::new(
        c.clone(),
        client.unwrap(),
        fr_client::native::observation_offer(),
        cfg.transport,
        Duration::from_secs(2),
    )
    .unwrap();
    let mut host = crate::session_startup::Host::start(
        h.clone(),
        native.unwrap(),
        Peer::Fixture {
            alive: Arc::new(AtomicBool::new(true)),
            until: now(h).unwrap() + 30_000_000,
            control: false,
        },
        cfg,
    )
    .unwrap();
    while !host.is_complete() || !viewer.is_complete() {
        let (a, b) = Box::pin(support::both(
            host.drive(Duration::from_millis(1)),
            viewer.drive(Duration::from_millis(1)),
        ))
        .await;
        a.unwrap();
        b.unwrap();
    }
    let host = host.finish().unwrap().into_running().unwrap();
    assert!(quality_decision::selected(host.selection()));
    let viewer = viewer.finish().unwrap();
    assert!(quality_decision::selected(&viewer.metadata().selection));
    (host, viewer)
}

#[test]
// One session owns startup, adaptive capture, reporting, renewal and revocation.
#[allow(clippy::too_many_lines)]
fn quality_reports_cross_the_original_running_host_control_channel() {
    use fr_wire::{
        Kind,
        input::{InputDelivery, InputDirection},
        quality_decision::{self, Decision},
    };
    run(|c, h| async move {
        let (mut session, mut viewer) = Box::pin(reporting_sessions(&c, &h)).await;
        let parent = session.binding();
        let initial_authority = viewer.metadata().observation_until_us;
        let Media {
            stream,
            channels,
            mut receiver,
            presenter: _,
        } = Box::pin(media(
            &mut session,
            &mut viewer,
            &c,
            &h,
            "unchanged",
            SendPolicy::default(),
        ))
        .await;
        let mut view = channels.binding();
        view.parent = parent;
        let incoming = Route::Stream(viewer.io().unwrap().1.inbound);
        let mut host = session.into_streaming(stream).unwrap();
        host.enable_adaptive_capture(Duration::from_millis(200))
            .unwrap();
        let original_worker = host.worker_id();
        let stop = host.stream.control.clone();
        let until = now(&h).unwrap() + 3_300_000;
        let mut n = 2000;
        let (result, reports) =
            Box::pin(support::both(
                host.serve(|| nonce(&mut n), || None, block),
                async {
                    let mut reports: Vec<Decision> = Vec::with_capacity(16);
                    let mut frames = 0;
                    while now(&h).unwrap() < until {
                        viewer
                            .drive(Duration::from_millis(1), |route, bytes| {
                                if bytes.get(6..8)
                                    != Some(&(Kind::QualityDecision as u16).to_be_bytes())
                                {
                                    return block(route, bytes);
                                }
                                assert_eq!(route, incoming);
                                let decision = quality_decision::decode(
                                    bytes,
                                    view,
                                    channels.limits().protocol(),
                                    InputDirection::HostToViewer,
                                    InputDelivery::Reliable,
                                )
                                .unwrap();
                                assert!(reports.last().is_none_or(|previous| {
                                    previous.sequence < decision.sequence
                                }));
                                assert!(reports.len() < 16, "bounded reporting, no idle flood");
                                assert_eq!(decision.applied_bitrate_bps, None);
                                assert_eq!(decision.pending_bitrate_bps, None);
                                reports.push(decision);
                                Ok(Disposition::Consumed)
                            })
                            .await
                            .unwrap();
                        frames += receive(&mut viewer, &channels, &mut receiver, &c).await;
                    }
                    assert_eq!(frames, 0, "diagnostics cannot invent encoded idle video");
                    assert!(now(&h).unwrap() > initial_authority);
                    assert!(
                        stop.check().is_ok(),
                        "ordinary observation renewal stays live"
                    );
                    assert_eq!(viewer.metadata().binding, parent);
                    assert_eq!(
                        viewer.metadata().selection.role,
                        fr_wire::negotiation::Role::Observe
                    );
                    stop.revoke();
                    reports
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(
            stop.check().is_err(),
            "reports cannot resurrect revoked authority"
        );
        assert_eq!(host.worker_id(), original_worker);
        assert_eq!(host.statistics().encoded_updates, 0);
        assert_eq!(reports.first().map(|report| report.sequence), Some(1));
        assert!(
            reports
                .iter()
                .any(|report| report.mode == quality_decision::Mode::Active)
        );
        let last = reports
            .last()
            .expect("the installed source must emit quality reports");
        let pacing = host.pacing().unwrap();
        assert_eq!(last.capture_interval_us, 200_000);
        assert_eq!(u64::from(last.capture_interval_us), pacing.interval_us());
        assert_eq!(last.mode, quality_decision::Mode::Idle);
        assert_eq!(last.reason, quality_decision::Reason::VerifiedIdle);
        assert_eq!(pacing.report().unwrap().mode, Mode::Idle);
        host.reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

#[test]
fn verified_idle_pacing_keeps_renewal_live_without_dummy_video() {
    run(|c, h| async move {
        let (mut host, mut viewer, channels, mut receiver) =
            Box::pin(fixture(&c, &h, "unchanged", SendPolicy::default())).await;
        host.enable_adaptive_capture(Duration::from_millis(200))
            .unwrap();
        let stop = host.stream.control.clone();
        let until = now(&c).unwrap() + 3_300_000;
        let mut n = 2000;
        let (result, frames) = Box::pin(support::both(
            host.serve(|| nonce(&mut n), || None, block),
            async {
                let mut frames = 0;
                while now(&c).unwrap() < until {
                    frames += receive(&mut viewer, &channels, &mut receiver, &c).await;
                }
                assert!(stop.check().is_ok(), "idle must still renew observation");
                stop.revoke();
                frames
            },
        ))
        .await;
        assert!(result.is_err());
        assert_eq!(frames, 0);
        assert_eq!(host.statistics().encoded_updates, 0);
        assert!((15..56).contains(&host.statistics().unchanged_observations));
        let controller = host.pacing().unwrap();
        assert_eq!(controller.interval_us(), 200_000);
        assert_eq!(controller.report().unwrap().mode, Mode::Idle);
        assert!(
            controller
                .decisions()
                .any(|r| r.reason == Reason::VerifiedIdle)
        );
        assert!(
            !controller
                .decisions()
                .any(|r| r.reason == Reason::HeadroomProbe)
        );
        host.reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

#[test]
fn first_changed_capture_after_idle_resumes_the_original_chain_without_catch_up() {
    run(|c, h| async move {
        let (mut host, mut viewer, channels, mut receiver) =
            Box::pin(fixture(&c, &h, "wake", SendPolicy::default())).await;
        host.enable_adaptive_capture(Duration::from_millis(200))
            .unwrap();
        let pid = host.worker_id();
        let stop = host.stream.control.clone();
        let until = now(&c).unwrap() + 2_500_000;
        let mut n = 2000;
        let (result, frames) = Box::pin(support::both(
            host.serve(|| nonce(&mut n), || None, block),
            async {
                let mut frames = 0;
                while now(&c).unwrap() < until {
                    frames += receive(&mut viewer, &channels, &mut receiver, &c).await;
                }
                stop.revoke();
                frames
            },
        ))
        .await;
        assert!(result.is_err());
        assert!(frames >= 3);
        assert_eq!(host.worker_id(), pid);
        let controller = host.pacing().unwrap();
        let reports: Vec<_> = controller.decisions().collect();
        let idle = reports
            .iter()
            .position(|r| r.reason == Reason::VerifiedIdle)
            .unwrap();
        let wake = reports
            .iter()
            .position(|r| r.reason == Reason::ChangedAfterIdle)
            .unwrap();
        assert!(wake > idle);
        assert_eq!(reports[wake].interval_us, 66_668);
        assert_eq!(controller.report().unwrap().mode, Mode::Active);
        assert!(
            host.statistics().encoded_updates < 25,
            "no accumulated idle capture burst"
        );
        host.reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

#[test]
fn source_work_pressure_reduces_future_capture_without_blocking_session_service() {
    run(|c, h| async move {
        let (mut host, mut viewer, channels, mut receiver) =
            Box::pin(fixture(&c, &h, "overloaded", SendPolicy::default())).await;
        host.enable_adaptive_capture(Duration::from_millis(200))
            .unwrap();
        let stop = host.stream.control.clone();
        let until = now(&c).unwrap() + 1_300_000;
        let mut n = 2000;
        let (result, frames) = Box::pin(support::both(
            host.serve(|| nonce(&mut n), || None, block),
            async {
                let mut frames = 0;
                while now(&c).unwrap() < until {
                    frames += receive(&mut viewer, &channels, &mut receiver, &c).await;
                }
                assert!(stop.check().is_ok());
                stop.revoke();
                frames
            },
        ))
        .await;
        assert!(result.is_err());
        assert!(frames >= 4, "pacing may not block decoding or QUIC service");
        let controller = host.pacing().unwrap();
        assert!(controller.interval_us() >= 133_336);
        assert!(
            controller
                .decisions()
                .any(|r| r.reason == Reason::SourceWork)
        );
        assert!(
            !controller
                .decisions()
                .any(|r| r.reason == Reason::VerifiedIdle)
        );
        host.reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

#[test]
fn private_capture_recovers_after_source_pressure_on_the_original_session_and_worker() {
    run(|c, h| async move {
        let (mut host, mut viewer, channels, mut receiver) =
            Box::pin(fixture(&c, &h, "recover-load", SendPolicy::default())).await;
        host.enable_adaptive_capture(Duration::from_millis(200))
            .unwrap();
        let original_worker = host.worker_id();
        let stop = host.stream.control.clone();
        let until = now(&c).unwrap() + 3_800_000;
        let mut n = 2000;
        let (result, frames) = Box::pin(support::both(
            host.serve(|| nonce(&mut n), || None, block),
            async {
                let mut frames = 0;
                while now(&c).unwrap() < until {
                    frames += receive(&mut viewer, &channels, &mut receiver, &c).await;
                }
                assert!(stop.check().is_ok(), "slowdown/recovery must keep renewing");
                stop.revoke();
                frames
            },
        ))
        .await;
        assert!(result.is_err());
        assert!(frames > 10);
        assert_eq!(host.worker_id(), original_worker);
        let controller = host.pacing().unwrap();
        let reduced = controller
            .decisions()
            .filter(|report| report.reason == Reason::SourceWork)
            .map(|report| report.interval_us)
            .max()
            .expect("the actual delayed worker must reduce capture admission");
        assert!(reduced >= 133_336);
        assert!(
            controller
                .decisions()
                .any(|report| report.reason == Reason::HeadroomProbe)
        );
        assert!(controller.interval_us() < reduced);
        host.reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

#[test]
fn empty_original_egress_does_not_report_ready_before_raw_capture_admission() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair_initialized(&c, &h, capabilities(), |_| {}).await;
        let Media { mut stream, .. } = Box::pin(media(
            &mut host,
            &mut viewer,
            &c,
            &h,
            "healthy",
            SendPolicy::default(),
        ))
        .await;
        stream
            .enable_adaptive_capture(Duration::from_millis(200))
            .unwrap();
        let worker = stream.worker_id();
        let control = stream.control.clone();
        let (credit, mut requests) = mpsc::channel(1);
        let (_completed, results) = mpsc::channel(1);
        let current = now(&h).unwrap();
        let mut other = block;
        let mut services = VideoServices {
            control: &control,
            sender: &mut stream.sender,
            statistics: &mut stream.statistics,
            policy: stream.policy,
            capacity: stream.capacity,
            credit,
            results,
            in_flight: None,
            native_target_check: None,
            send_evidence: send_evidence::SendEvidence::default(),
            pacing: stream.pacing.as_mut(),
            bitrate: None,
            rate_in_flight: None,
            last_work: None,
            last_capture: None,
            next_capture: current + 100_000,
            input_wake: super::super::super::input_wake::Wake::default(),
            feedback: None,
            quality: None,
            presentation: None,
            cursor: None,
            media: None,
            other: &mut other,
        };
        let mut n = 2000;
        services
            .maintain(host.io().unwrap().0, &mut || nonce(&mut n))
            .unwrap();
        assert_eq!(services.statistics.admitted_records, 0);
        assert_eq!(
            services
                .pacing
                .as_deref()
                .unwrap()
                .report()
                .unwrap()
                .sample
                .send,
            Availability::Unknown,
            "empty sender polls cannot claim a successful transport admission"
        );
        assert!(matches!(requests.try_recv(), Err(mpsc::RecvError::Empty)));
        drop(services);
        assert_eq!(stream.worker_id(), worker);
        host.close();
        stream.close();
        stream
            .reap(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

#[test]
fn retained_reference_credit_is_not_reported_as_idle_or_network_capacity() {
    run(|c, h| async move {
        let (mut host, mut viewer, channels, mut receiver) = Box::pin(fixture(
            &c,
            &h,
            "healthy",
            SendPolicy {
                max_cached_pictures: 1,
                ..SendPolicy::default()
            },
        ))
        .await;
        host.enable_adaptive_capture(Duration::from_millis(200))
            .unwrap();
        let stop = host.stream.control.clone();
        let until = now(&c).unwrap() + 320_000;
        let mut n = 2000;
        let (result, frames) = Box::pin(support::both(
            host.serve(|| nonce(&mut n), || None, block),
            async {
                let mut frames = 0;
                while now(&c).unwrap() < until {
                    frames += receive(&mut viewer, &channels, &mut receiver, &c).await;
                }
                stop.revoke();
                frames
            },
        ))
        .await;
        assert!(result.is_err());
        assert_eq!(frames, 0);
        assert_eq!(host.statistics().encoded_updates, 0);
        assert_eq!(host.statistics().unchanged_observations, 0);
        let controller = host.pacing().unwrap();
        assert!(
            controller
                .decisions()
                .any(|r| r.reason == Reason::CaptureCredit)
        );
        assert!(!controller.decisions().any(|r| matches!(
            r.reason,
            Reason::VerifiedIdle | Reason::SendAdmission | Reason::HeadroomProbe
        )));
        assert_eq!(controller.report().unwrap().sample.source_work_us, None);
        host.reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

#[test]
fn pacing_configuration_cannot_replace_a_running_or_stopped_controller() {
    run(|c, h| async move {
        let (mut host, _, _, _) = Box::pin(fixture(&c, &h, "healthy", SendPolicy::default())).await;
        assert!(
            host.enable_adaptive_capture(Duration::from_millis(20))
                .is_err()
        );
        assert!(host.pacing().is_none());
        host.enable_adaptive_capture(Duration::from_millis(200))
            .unwrap();
        assert!(
            host.enable_adaptive_capture(Duration::from_millis(150))
                .is_err()
        );
        assert_eq!(host.pacing().unwrap().policy().maximum_interval_us, 200_000);
        drop(host.serve(|| Ok(1), || None, block));
        assert!(
            host.enable_adaptive_capture(Duration::from_millis(200))
                .is_err()
        );
        host.reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}
