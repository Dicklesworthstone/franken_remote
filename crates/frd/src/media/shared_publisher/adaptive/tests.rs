use super::*;

fn pacing() -> CapturePacing {
    CapturePacing::new(Duration::from_millis(50), Duration::from_millis(200), 30).unwrap()
}
fn complete(pacing: &mut CapturePacing, now: u64, changed: bool) {
    pacing.start(now - 1000).unwrap();
    pacing.finish(now, now - 1000, changed).unwrap();
}
#[test]
fn one_healthy_viewer_outvotes_a_blocked_viewer_without_inventing_delivery() {
    let blocked = Some((100_000, Availability::Blocked));
    let ready = Some((100_000, Availability::Ready));
    for samples in [[blocked, ready], [ready, blocked], [None, ready]] {
        assert_eq!(
            send_evidence(110_000, samples.into_iter()),
            Availability::Ready
        );
    }
    assert_eq!(
        send_evidence(110_000, [blocked, blocked].into_iter()),
        Availability::Blocked
    );
    assert_eq!(
        send_evidence(110_000, [blocked, None].into_iter()),
        Availability::Unknown
    );
    assert_eq!(
        send_evidence(110_000, [].into_iter()),
        Availability::Unknown
    );
}
#[test]
fn silent_stale_or_future_send_samples_cannot_assert_headroom_or_global_pressure() {
    for availability in [Availability::Ready, Availability::Blocked] {
        assert_eq!(
            send_evidence(200_001, [Some((100_000, availability))].into_iter()),
            Availability::Unknown
        );
        assert_eq!(
            send_evidence(99_999, [Some((100_000, availability))].into_iter()),
            Availability::Unknown
        );
    }
}
#[test]
fn replaced_view_cannot_inherit_enqueue_evidence_before_its_first_new_send() {
    let original = binding();
    let mut replacement = original;
    replacement.recovery = replacement.recovery.next().unwrap();
    for availability in [Availability::Ready, Availability::Blocked] {
        let sample = SendSample {
            view: original,
            at: 10_000,
            availability,
        };
        assert_eq!(sample.current(original), Some((10_000, availability)));
        assert_eq!(
            send_evidence(10_001, [sample.current(replacement)].into_iter()),
            Availability::Unknown
        );
        let current = SendSample {
            view: replacement,
            ..sample
        };
        assert_eq!(
            send_evidence(10_001, [current.current(replacement)].into_iter()),
            availability
        );
    }
}
fn binding() -> Binding {
    use fr_core::ids::*;
    Binding {
        parent: fr_wire::negotiation::ControlBinding {
            id: 1,
            host_boot: HostBootId::from_raw(2),
            os_session: OsSessionId::from_raw(3),
            remote_session: RemoteSessionId::from_raw(4),
        },
        display: 5,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
#[test]
fn repeated_idle_service_does_not_refresh_stale_enqueue_evidence() {
    let view = binding();
    for availability in [Availability::Ready, Availability::Blocked] {
        let mut sample = Some(SendSample {
            view,
            at: 10_000,
            availability,
        });
        for at in [50_000, 90_000, 110_001] {
            record_send(&mut sample, view, at, &crate::media_egress::Progress::Idle);
        }
        let current = sample.and_then(|sample| sample.current(view));
        assert_eq!(current, Some((10_000, availability)));
        assert_eq!(
            send_evidence(110_001, [current].into_iter()),
            Availability::Unknown
        );
        let mut absent = None;
        record_send(
            &mut absent,
            view,
            110_001,
            &crate::media_egress::Progress::Idle,
        );
        assert!(absent.is_none());
    }
}
#[test]
fn own_capture_reservation_is_unknown_credit_and_live_work_age_is_measured() {
    let mut pacing = pacing();
    pacing.start(1000).unwrap();
    assert_eq!(pacing.start(1001), Err(Error::InvalidBudget));
    pacing.sample(71_000, false, Availability::Ready).unwrap();
    let report = pacing.controller.report().unwrap();
    assert_eq!(report.sample.capture_credit, Availability::Unknown);
    assert_eq!(report.sample.source_work_us, Some(70_000));
    pacing.finish(81_000, 2000, true).unwrap();
    pacing.sample(82_000, true, Availability::Ready).unwrap();
    assert_eq!(
        pacing.controller.report().unwrap().sample.source_work_us,
        Some(80_000)
    );
    pacing.sample(331_001, true, Availability::Ready).unwrap();
    assert_eq!(
        pacing.controller.report().unwrap().sample.source_work_us,
        None
    );
}
#[test]
fn shared_pool_pressure_reduces_raw_work_after_dwell_within_the_operator_floor() {
    let mut pacing = pacing();
    for now in (0..=400_000).step_by(10_000) {
        pacing.sample(now, false, Availability::Ready).unwrap();
    }
    assert_eq!(pacing.controller.interval_us(), 200_000);
    assert!(
        pacing
            .controller
            .decisions()
            .any(|r| r.reason == pacing::Reason::CaptureCredit)
    );
    assert!(
        pacing
            .controller
            .decisions()
            .all(|r| (50_000..=200_000).contains(&r.interval_us))
    );
}
#[test]
fn source_pressure_stays_distinct_from_shared_pool_or_send_pressure() {
    let mut pacing = pacing();
    pacing.start(0).unwrap();
    for now in (0..=250_000).step_by(10_000) {
        pacing.sample(now, false, Availability::Ready).unwrap();
    }
    assert_eq!(pacing.controller.interval_us(), 200_000);
    assert!(
        pacing
            .controller
            .decisions()
            .any(|r| r.reason == pacing::Reason::SourceWork)
    );
    assert!(
        !pacing
            .controller
            .decisions()
            .any(|r| r.reason == pacing::Reason::CaptureCredit)
    );
}
#[test]
fn only_actual_unchanged_observations_establish_idle_and_actual_change_wakes_it() {
    let mut observed = pacing();
    let mut silent = pacing();
    for now in (10_000_u64..=1_300_000).step_by(10_000) {
        if now.is_multiple_of(100_000) {
            complete(&mut observed, now, false);
        }
        observed.sample(now, true, Availability::Ready).unwrap();
        silent.sample(now, true, Availability::Ready).unwrap();
    }
    assert_eq!(
        observed.controller.report().unwrap().mode,
        pacing::Mode::Idle
    );
    assert_eq!(observed.controller.interval_us(), 200_000);
    assert_eq!(
        silent.controller.report().unwrap().mode,
        pacing::Mode::Active
    );
    complete(&mut observed, 1_310_000, true);
    observed
        .sample(1_310_000, true, Availability::Ready)
        .unwrap();
    assert_eq!(observed.controller.interval_us(), 100_000);
    assert_eq!(
        observed.controller.report().unwrap().reason,
        pacing::Reason::ChangedAfterIdle
    );
}
#[test]
fn missing_send_evidence_never_allows_an_upward_probe() {
    let mut pacing = pacing();
    for now in (0..=150_000).step_by(10_000) {
        pacing.sample(now, false, Availability::Ready).unwrap();
    }
    for now in (160_000_u64..=3_000_000).step_by(10_000) {
        if now.is_multiple_of(100_000) {
            complete(&mut pacing, now, true);
        }
        pacing.sample(now, true, Availability::Unknown).unwrap();
    }
    assert_eq!(pacing.controller.interval_us(), 200_000);
    assert!(
        !pacing
            .controller
            .decisions()
            .any(|r| r.reason == pacing::Reason::HeadroomProbe)
    );
}
#[test]
fn source_clock_faults_and_impossible_observations_are_typed_refusals() {
    for (completed, observed) in [(99, 100), (120, 99), (120, 121)] {
        let mut pacing = pacing();
        pacing.start(100).unwrap();
        assert!(pacing.finish(completed, observed, false).is_err());
    }
    let mut pacing = pacing();
    pacing.start(100).unwrap();
    assert_eq!(
        pacing.sample(99, true, Availability::Ready),
        Err(pacing_error(pacing::Error::ClockRegression))
    );
}
#[test]
fn policy_preserves_fps_fractional_rounding_and_slow_operator_choices() {
    assert!(
        CapturePacing::new(
            Duration::from_micros(33_333),
            Duration::from_millis(200),
            30
        )
        .is_err()
    );
    assert!(CapturePacing::new(Duration::from_millis(50), Duration::from_millis(201), 30).is_err());
    let rounded = CapturePacing::new(
        Duration::from_nanos(33_334_001),
        Duration::from_millis(200),
        30,
    )
    .unwrap();
    assert_eq!(rounded.controller.policy().minimum_interval_us, 33_335);
    let slow = CapturePacing::new(Duration::from_secs(1), Duration::from_secs(1), 1).unwrap();
    assert_eq!(slow.controller.interval_us(), 1_000_000);
}
