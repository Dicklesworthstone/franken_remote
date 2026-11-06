//! Real pacing/bitrate state and the production sender with explicit admission
//! outcomes. Native encoder and network qualification are separate tests.
use super::*;
use asupersync::net::quic_native::StreamId;
use fr_core::ids::*;
use fr_transport::quic::{Messages, Priority, StreamRoute};

fn view() -> Binding {
    Binding {
        parent: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(3),
        },
        display: 4,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
fn sender() -> Sender {
    let host =
        crate::session_startup::with_quality_decisions(crate::session_startup::host_offer(false))
            .unwrap();
    let selection = host
        .intersect(&fr_client::native::observation_offer())
        .unwrap()
        .select()
        .unwrap();
    let view = view();
    Sender::selected(
        &selection,
        view.parent,
        view,
        Route::Stream(StreamRoute {
            stream: StreamId(3),
            binding: 7,
            messages: Messages::SessionControl,
            priority: Priority::Critical,
            outbound: true,
            maximum: 4096,
        }),
    )
    .unwrap()
    .unwrap()
}
fn point(interval_us: u32) -> Point {
    Point {
        interval_us,
        mode: Mode::Active,
        reason: Reason::SourceWork,
        applied: None,
        pending: None,
    }
}
fn sample(now: u64, send: pacing::Availability) -> pacing::Sample {
    pacing::Sample {
        now_us: now,
        source_work_us: Some(40_000),
        send,
        capture_credit: pacing::Availability::Ready,
        observation: Some(pacing::Observation {
            at_us: now,
            changed: true,
        }),
    }
}

#[test]
fn absent_source_evidence_is_unknown_and_hold_preserves_the_actual_cadence_reason() {
    let mut pacing = pacing::Controller::new(pacing::Policy {
        minimum_interval_us: 10_000,
        maximum_interval_us: 200_000,
    })
    .unwrap();
    assert_eq!(Point::current(&pacing, None), None);
    for at in [0, 50_000, 100_000] {
        pacing
            .update(sample(at, pacing::Availability::Ready))
            .unwrap();
    }
    let pressure = Point::current(&pacing, None).unwrap();
    assert_eq!(pressure.reason, Reason::SourceWork);
    let held = pacing
        .update(sample(120_000, pacing::Availability::Ready))
        .unwrap();
    assert_eq!(held.reason, pacing::Reason::Hold);
    assert_eq!(Point::current(&pacing, None), Some(pressure));
    assert_eq!(pressure.applied, None);
}

#[test]
fn bitrate_intent_is_never_reported_as_an_applied_native_change() {
    let mut pacing = pacing::Controller::new(pacing::Policy {
        minimum_interval_us: 10_000,
        maximum_interval_us: 200_000,
    })
    .unwrap();
    let mut bitrate = bitrate::Controller::new(2_000_000).unwrap();
    for at in [0, 50_000, 100_000] {
        let report = pacing
            .update(sample(at, pacing::Availability::Blocked))
            .unwrap();
        bitrate.observe(&report).unwrap();
    }
    let pending = Point::current(&pacing, Some(&bitrate)).unwrap();
    assert_eq!(
        (pending.applied, pending.pending),
        (Some(2_000_000), Some(1_000_000))
    );
    let first = bitrate.begin_capture().unwrap();
    bitrate
        .complete_capture(first, 2_000_000, false, 110_000)
        .unwrap();
    assert_eq!(Point::current(&pacing, Some(&bitrate)), Some(pending));
    let second = bitrate.begin_capture().unwrap();
    bitrate
        .complete_capture(second, 1_000_000, true, 120_000)
        .unwrap();
    let applied = Point::current(&pacing, Some(&bitrate)).unwrap();
    assert_eq!((applied.applied, applied.pending), (Some(1_000_000), None));
}

#[test]
fn backpressure_keeps_no_stale_queue_and_admits_only_the_latest_point() {
    let mut sender = sender();
    assert!(
        !sender
            .transmit(Some(point(20_000)), 0, |d, until| {
                assert_eq!((d.sequence, until), (1, ADMISSION_LIFETIME_US));
                Ok(Admission::Backpressure)
            })
            .unwrap()
    );
    assert!(
        !sender
            .transmit(Some(point(40_000)), 1, |_, _| panic!("bounded cadence"))
            .unwrap()
    );
    assert!(
        sender
            .transmit(Some(point(80_000)), REPORT_INTERVAL_US, |d, until| {
                assert_eq!((d.sequence, d.capture_interval_us), (1, 80_000));
                assert_eq!(until, REPORT_INTERVAL_US + ADMISSION_LIFETIME_US);
                Ok(Admission::Accepted)
            })
            .unwrap()
    );
    assert_eq!(sender.accepted, Some(point(80_000)));
    assert!(
        !sender
            .transmit(Some(point(80_000)), 2 * REPORT_INTERVAL_US, |_, _| {
                panic!("unchanged state cannot create idle traffic")
            })
            .unwrap()
    );
}

#[test]
fn expiry_is_not_admission_and_terminal_admission_failures_are_preserved() {
    let mut sender = sender();
    assert!(
        !sender
            .transmit(Some(point(20_000)), 0, |_, _| Ok(Admission::Expired))
            .unwrap()
    );
    assert_eq!(sender.next, 1);
    assert_eq!(sender.accepted, None);
    let error = Error::Admission(super::super::receiver_feedback::Error::Closed);
    assert_eq!(
        sender.transmit(Some(point(40_000)), REPORT_INTERVAL_US, |_, _| Err(error)),
        Err(error)
    );
    assert_eq!(sender.next, 1);
    assert_eq!(sender.accepted, None);
}

#[test]
fn recovery_rebinds_media_without_resetting_sequence_or_report_rate() {
    let mut sender = sender();
    assert!(
        sender
            .transmit(Some(point(20_000)), 0, |d, _| {
                assert_eq!(d.sequence, 1);
                Ok(Admission::Accepted)
            })
            .unwrap()
    );
    let mut recovered = view();
    recovered.recovery = RecoveryGeneration::INITIAL.next().unwrap();
    recovered.parent.id = 9; // new media attachment, same original session
    sender.rebind(recovered).unwrap();
    assert_eq!(sender.view.parent.id, 7);
    assert_eq!(sender.accepted, None);
    assert!(
        !sender
            .transmit(Some(point(20_000)), 1, |_, _| panic!(
                "no report flood at recovery"
            ))
            .unwrap()
    );
    assert!(
        sender
            .transmit(Some(point(20_000)), REPORT_INTERVAL_US, |d, _| {
                assert_eq!(d.sequence, 2);
                Ok(Admission::Accepted)
            })
            .unwrap()
    );
    let mut foreign = recovered;
    foreign.parent.remote_session = RemoteSessionId::from_raw(30);
    let before = sender.view;
    assert_eq!(sender.rebind(foreign), Err(Error::Binding));
    assert_eq!(sender.view, before);
}

#[test]
fn unknown_state_and_clock_faults_do_not_emit_reports() {
    let mut sender = sender();
    assert!(
        !sender
            .transmit(None, 10, |_, _| panic!("no fabricated source"))
            .unwrap()
    );
    assert_eq!(
        sender.transmit(Some(point(20_000)), 9, |_, _| panic!("regressed clock")),
        Err(Error::Clock)
    );
    assert_eq!(
        sender.transmit(Some(point(20_000)), u64::MAX, |_, _| panic!("overflow")),
        Err(Error::Clock)
    );
    assert_eq!(sender.next, 1);
}

#[test]
fn reporting_requires_selected_decoder_and_original_outbound_control_route() {
    let host =
        crate::session_startup::with_quality_decisions(crate::session_startup::host_offer(false))
            .unwrap();
    let mut selection = host
        .intersect(&fr_client::native::observation_offer())
        .unwrap()
        .select()
        .unwrap();
    let view = view();
    let Route::Stream(route) = sender().route else {
        panic!("control stream")
    };
    for invalid in [
        StreamRoute {
            binding: route.binding + 1,
            ..route
        },
        StreamRoute {
            outbound: false,
            ..route
        },
        StreamRoute {
            priority: Priority::Bulk,
            ..route
        },
        StreamRoute {
            messages: Messages::DecoderReplies,
            ..route
        },
        StreamRoute {
            maximum: wire::RECORD_BYTES - 1,
            ..route
        },
    ] {
        assert!(matches!(
            Sender::selected(&selection, view.parent, view, Route::Stream(invalid)),
            Err(Error::Binding)
        ));
    }
    selection
        .capabilities
        .retain(|cap| cap.name != fr_wire::decoder::CAPABILITY);
    assert!(matches!(
        Sender::selected(&selection, view.parent, view, Route::Stream(route)),
        Err(Error::Binding)
    ));
    selection
        .capabilities
        .retain(|cap| cap.name != wire::CAPABILITY);
    assert!(
        Sender::selected(&selection, view.parent, view, Route::Stream(route))
            .unwrap()
            .is_none()
    );
}
