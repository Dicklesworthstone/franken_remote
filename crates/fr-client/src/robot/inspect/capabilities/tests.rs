use super::*;
use fr_core::limits::{LimitOverrides, ProtocolLimits};
use fr_wire::negotiation::{Capability, MAX_CAPABILITIES};

fn peer() -> RobotInspectData {
    RobotInspectData {
        host: "n-host".into(),
        certificate_name: "workstation.fixture.ts.net".into(),
        addresses: vec!["100.64.0.2".into()],
        port: 8443,
        is_derp_relayed: false,
        state: "discovered".into(),
        displays_count: Some(4),
        requires_approval: Some(false),
        transport_path: "unknown".into(),
    }
}

#[test]
fn native_probe_advertises_understanding_without_requesting_authority() {
    let offer = crate::native::inspection_offer().unwrap();
    offer.validate().unwrap();
    assert_eq!(offer.role, Role::Observe);
    assert!(offer.capabilities.len() <= MAX_CAPABILITIES);
    let required = offer
        .capabilities
        .iter()
        .filter(|capability| capability.required)
        .collect::<Vec<_>>();
    assert_eq!(required.len(), 1);
    assert_eq!(required[0].name, inspection::CAPABILITY);
    for name in [
        fr_wire::attachment::INPUT_CAPABILITY,
        fr_wire::clipboard::CAPABILITY,
        fr_wire::files::CAPABILITY,
        fr_wire::audio::CAPABILITY,
    ] {
        assert!(
            offer
                .capabilities
                .iter()
                .any(|capability| capability.name == name && !capability.required)
        );
    }
}

#[test]
fn report_serializes_only_the_limits_actually_advertised_on_the_wire() {
    let mut offer = crate::native::inspection_offer().unwrap();
    offer.limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(4096),
        max_clipboard_item_bytes: Some(8192),
        max_encoded_access_unit_bytes: Some(16384),
        max_dimension_pixels: Some(2048),
        max_coded_pixels: Some(1_048_576),
        reassembly_window_pictures: Some(2),
        per_viewer_compressed_bytes: Some(32768),
        ..Default::default()
    })
    .unwrap();
    let report = CapabilityInspectionData::after_acknowledged_probe(peer(), offer).unwrap();
    let value = serde_json::to_value(&report).unwrap();
    assert_eq!(value["scope"], "advertised_capability_intersection");
    assert_eq!(value["completion"], "inspection_acknowledged");
    assert_eq!(value["state"], "capabilities_observed");
    assert!(value["displays_count"].is_null());
    assert!(value["requires_approval"].is_null());
    for field in [
        "observation_opened",
        "input_requested",
        "native_workers_started",
        "live_capability_admission_verified",
    ] {
        assert_eq!(value[field], false, "{field}");
    }
    assert_eq!(
        value["limits"],
        serde_json::json!({
            "max_control_message_bytes": 4096,
            "max_clipboard_item_bytes": 8192,
            "max_encoded_access_unit_bytes": 16384,
            "max_dimension_pixels": 2048,
            "max_coded_pixels": 1_048_576,
            "reassembly_window_pictures": 2,
            "per_viewer_compressed_bytes": 32768
        })
    );
    assert_eq!(
        serde_json::from_value::<CapabilityInspectionData>(value).unwrap(),
        report
    );
    let human = report.render_human();
    assert!(human.contains("advertised intersection"));
    assert!(human.contains("acknowledged and closed before observation"));
    assert!(human.contains("approval policy and live codec admission remain unknown"));
}

#[test]
fn invalid_or_unmarked_capability_reports_cannot_become_success() {
    let valid = crate::native::inspection_offer().unwrap();
    let mut cases = Vec::new();
    let mut missing = valid.clone();
    missing
        .capabilities
        .retain(|capability| capability.name != inspection::CAPABILITY);
    cases.push(missing);
    for required in [false, true] {
        let mut wrong = valid.clone();
        let marker = wrong
            .capabilities
            .iter_mut()
            .find(|capability| capability.name == inspection::CAPABILITY)
            .unwrap();
        marker.required = required;
        marker.version = if required {
            inspection::VERSION + 1
        } else {
            inspection::VERSION
        };
        cases.push(wrong);
    }
    let mut control = valid.clone();
    control.role = Role::RequestControl;
    cases.push(control);
    let mut duplicate = valid;
    duplicate.capabilities.push(Capability {
        name: inspection::CAPABILITY.into(),
        version: inspection::VERSION,
        required: true,
    });
    cases.push(duplicate);
    for invalid in cases {
        assert!(CapabilityInspectionData::after_acknowledged_probe(peer(), invalid).is_err());
    }
}
