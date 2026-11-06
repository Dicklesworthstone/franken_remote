use super::*;
use asupersync::net::quic_native::StreamId;
use fr_core::ids::*;
use fr_wire::{
    decoder,
    negotiation::{Capability, Role},
    quality_decision::{Mode, Reason},
};

fn binding() -> Binding {
    Binding {
        parent: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(13),
        },
        display: 14,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
fn inbound() -> StreamRoute {
    StreamRoute {
        stream: StreamId(3),
        binding: binding().parent.id,
        messages: Messages::SessionControl,
        priority: Priority::Critical,
        outbound: false,
        maximum: 65_536,
    }
}
fn selection() -> Selection {
    Selection {
        version: 0,
        profile: fr_wire::negotiation::NATIVE_PROFILE,
        profile_version: fr_wire::negotiation::PROFILE_VERSION,
        role: Role::Observe,
        capabilities: [decoder::CAPABILITY, wire::CAPABILITY]
            .into_iter()
            .map(|name| Capability {
                name: name.into(),
                version: 1,
                required: false,
            })
            .collect(),
        limits: ProtocolLimits::ABSOLUTE,
    }
}
fn receiver() -> ViewerQuality {
    ViewerQuality::attach(&selection(), binding().parent, binding(), inbound())
        .unwrap()
        .unwrap()
}
fn decision(sequence: u64) -> Decision {
    Decision {
        sequence,
        capture_interval_us: 50_000,
        mode: Mode::Active,
        reason: Reason::SourceWork,
        applied_bitrate_bps: Some(2_000_000),
        pending_bitrate_bps: Some(1_500_000),
    }
}
fn encoded(binding: Binding, decision: Decision) -> [u8; wire::RECORD_BYTES] {
    let mut bytes = [0; wire::RECORD_BYTES];
    assert_eq!(
        wire::encode(
            decision,
            binding,
            &ProtocolLimits::ABSOLUTE,
            &mut bytes,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap(),
        bytes.len()
    );
    bytes
}

#[test]
fn quality_stays_unknown_without_positive_selection_and_a_real_record() {
    let mut selected = selection();
    selected.capabilities.pop();
    assert!(
        ViewerQuality::attach(&selected, binding().parent, binding(), inbound())
            .unwrap()
            .is_none()
    );
    let mut selected = selection();
    selected.capabilities[1].version = 2;
    assert!(matches!(
        ViewerQuality::attach(&selected, binding().parent, binding(), inbound()),
        Err(Fault::NotSelected)
    ));
    let mut selected = selection();
    selected.capabilities.remove(0);
    assert!(matches!(
        ViewerQuality::attach(&selected, binding().parent, binding(), inbound()),
        Err(Fault::NotSelected)
    ));
    let quality = receiver();
    assert_eq!(quality.current(), None);
    assert_eq!(quality.last(), None);
    assert_eq!(quality.received, 0);
}

#[test]
fn a_concurrent_snapshot_read_keeps_the_original_record_pending_without_advancing() {
    let mut quality = receiver();
    let route = Route::Stream(inbound());
    let first = encoded(binding(), decision(1));
    assert_eq!(quality.receive(route, &first), Ok(Disposition::Consumed));
    let shared = quality.shared.clone();
    let reading = shared.value.lock().unwrap();
    let next = encoded(binding(), decision(2));
    assert_eq!(quality.receive(route, &next), Ok(Disposition::Blocked));
    assert_eq!(quality.sequence, 1);
    assert_eq!(quality.received, 1);
    assert_eq!(reading.as_ref().unwrap().decision, decision(1));
    drop(reading);
    assert_eq!(quality.current(), Some(decision(1)));
    assert_eq!(quality.receive(route, &next), Ok(Disposition::Consumed));
    assert_eq!(quality.sequence, 2);
    assert_eq!(quality.received, 2);
    assert_eq!(quality.current(), Some(decision(2)));
}

#[test]
fn quality_uses_the_exact_parent_and_inbound_critical_control_lane() {
    let mut parent = binding().parent;
    parent.remote_session = RemoteSessionId::from_raw(99);
    assert!(matches!(
        ViewerQuality::attach(&selection(), parent, binding(), inbound()),
        Err(Fault::Binding)
    ));
    for route in [
        StreamRoute {
            binding: 99,
            ..inbound()
        },
        StreamRoute {
            outbound: true,
            ..inbound()
        },
        StreamRoute {
            priority: Priority::Bulk,
            ..inbound()
        },
        StreamRoute {
            messages: Messages::DecoderReplies,
            ..inbound()
        },
    ] {
        assert!(matches!(
            ViewerQuality::attach(&selection(), binding().parent, binding(), route),
            Err(Fault::Binding)
        ));
    }
    assert!(matches!(
        ViewerQuality::attach(
            &selection(),
            binding().parent,
            binding(),
            StreamRoute {
                maximum: wire::RECORD_BYTES - 1,
                ..inbound()
            }
        ),
        Err(Fault::Wire(WireError::ResourceLimit))
    ));
}

#[test]
fn reports_replace_one_snapshot_without_promoting_pending_bitrate_to_applied() {
    let mut quality = receiver();
    for sequence in 1..=1024 {
        let value = decision(sequence);
        assert_eq!(
            quality.receive(Route::Stream(inbound()), &encoded(binding(), value)),
            Ok(Disposition::Consumed)
        );
        assert_eq!(quality.current(), Some(value));
        assert_eq!(quality.received, sequence);
    }
    assert_eq!(
        quality.current().unwrap().applied_bitrate_bps,
        Some(2_000_000)
    );
    assert_eq!(
        quality.current().unwrap().pending_bitrate_bps,
        Some(1_500_000)
    );
    assert_eq!(
        quality.receive(
            Route::Stream(inbound()),
            &encoded(binding(), decision(1024))
        ),
        Err(Fault::Sequence)
    );
    assert_eq!(quality.received, 1024);
    assert_eq!(quality.current(), Some(decision(1024)));
}

#[test]
fn malformed_and_foreign_reports_cannot_replace_the_last_received_decision() {
    let mut quality = receiver();
    quality
        .receive(Route::Stream(inbound()), &encoded(binding(), decision(5)))
        .unwrap();
    let mut foreign = binding();
    foreign.display += 1;
    assert_eq!(
        quality.receive(Route::Stream(inbound()), &encoded(foreign, decision(6))),
        Err(Fault::Wire(WireError::InvalidBinding))
    );
    assert_eq!(
        quality.receive(
            Route::Stream(StreamRoute {
                stream: StreamId(7),
                ..inbound()
            }),
            &encoded(binding(), decision(6)),
        ),
        Err(Fault::Binding)
    );
    let mut malformed = encoded(binding(), decision(6));
    malformed[wire::RECORD_BYTES - 1] = 0xff;
    // A truncated record is always malformed, independent of its field values.
    assert!(matches!(
        quality.receive(Route::Stream(inbound()), &malformed[..malformed.len() - 1]),
        Err(Fault::Wire(_))
    ));
    assert_eq!(quality.current(), Some(decision(5)));
    assert_eq!(quality.received, 1);
}

#[test]
fn recovery_retires_only_validated_old_reports_and_holds_the_next_scope() {
    let mut quality = receiver();
    quality
        .receive(Route::Stream(inbound()), &encoded(binding(), decision(5)))
        .unwrap();
    quality.begin_recovery().unwrap();
    assert_eq!(quality.current(), None);
    assert_eq!(quality.last(), None);
    assert_eq!(
        quality.receive(Route::Stream(inbound()), &encoded(binding(), decision(6))),
        Ok(Disposition::Consumed)
    );
    assert_eq!(quality.received, 2);
    assert_eq!(
        quality.last(),
        None,
        "obsolete reports must stay out of current diagnostics"
    );
    assert!(matches!(
        quality.receive(
            Route::Stream(inbound()),
            &encoded(binding(), decision(7))[..149]
        ),
        Err(Fault::Wire(_))
    ));
    let next = Binding {
        recovery: binding().recovery.next().unwrap(),
        ..binding()
    };
    let bytes = encoded(next, decision(7));
    for _ in 0..3 {
        assert_eq!(
            quality.receive(Route::Stream(inbound()), &bytes),
            Ok(Disposition::Blocked)
        );
        assert_eq!(
            quality.sequence, 6,
            "backpressure cannot consume a sequence"
        );
    }
    quality
        .rebind(&selection(), next.parent, next, inbound())
        .unwrap();
    assert_eq!(quality.current(), None);
    assert_eq!(
        quality.receive(Route::Stream(inbound()), &bytes),
        Ok(Disposition::Consumed)
    );
    assert_eq!(quality.current(), Some(decision(7)));
    assert_eq!(quality.received, 3);
    assert_eq!(
        quality.receive(Route::Stream(inbound()), &encoded(next, decision(6))),
        Err(Fault::Sequence)
    );
    assert_eq!(
        quality.receive(Route::Stream(inbound()), &encoded(binding(), decision(8))),
        Err(Fault::Wire(WireError::InvalidBinding))
    );
}

#[test]
fn recovery_never_accepts_a_foreign_parent_or_unannounced_geometry() {
    let mut quality = receiver();
    quality.begin_recovery().unwrap();
    let next = Binding {
        recovery: binding().recovery.next().unwrap(),
        ..binding()
    };
    for foreign in [
        Binding {
            display: 99,
            ..next
        },
        Binding {
            geometry: next.geometry.next().unwrap(),
            ..next
        },
        Binding {
            configuration: next.configuration.next().unwrap(),
            ..next
        },
    ] {
        assert_eq!(
            quality.receive(Route::Stream(inbound()), &encoded(foreign, decision(1))),
            Err(Fault::Wire(WireError::InvalidBinding))
        );
        assert_eq!(
            quality.rebind(&selection(), foreign.parent, foreign, inbound()),
            Err(Fault::Binding)
        );
    }
    assert_eq!(quality.current(), None);
    assert_eq!(quality.received, 0);
}

#[test]
fn closure_makes_current_quality_unknown_and_retains_only_historical_diagnostics() {
    let mut quality = receiver();
    quality
        .receive(Route::Stream(inbound()), &encoded(binding(), decision(1)))
        .unwrap();
    quality.close();
    assert_eq!(quality.current(), None);
    assert_eq!(quality.last(), Some(decision(1)));
    assert_eq!(quality.received, 1);
    assert_eq!(quality.begin_recovery(), Err(Fault::Closed));
    assert_eq!(
        quality.receive(Route::Stream(inbound()), &encoded(binding(), decision(2))),
        Err(Fault::Closed)
    );
}
