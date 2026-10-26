//! Production session and QUIC owners, real supervised IPC, synthetic codec.
use super::*;
use fr_media::pacing::{Mode, Reason};

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
            send_evidence: send_evidence::SendEvidence::default(),
            pacing: stream.pacing.as_mut(),
            last_work: None,
            last_capture: None,
            next_capture: current + 100_000,
            input_wake: super::super::super::input_wake::Wake::default(),
            repair_turn: false,
            feedback: None,
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
