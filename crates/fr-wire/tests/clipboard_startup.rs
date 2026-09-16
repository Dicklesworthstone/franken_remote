//! Allocation-free control-lane readiness; all identities are explicit fixtures.
use fr_core::{
    clipboard::Binding,
    ids::*,
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::{clipboard::startup, negotiation::ControlBinding};
fn parent() -> ControlBinding {
    ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(11),
        os_session: OsSessionId::from_raw(12),
        remote_session: RemoteSessionId::from_raw(13),
    }
}
fn scope() -> Binding {
    Binding {
        session: parent().remote_session,
        lease: InputLeaseId::from_raw(19),
    }
}
#[test]
fn readiness_roundtrips_both_consent_values_and_has_fixed_length() {
    for consent in [false, true] {
        let mut bytes = [0; startup::RECORD_BYTES];
        assert_eq!(
            startup::encode(
                parent(),
                scope(),
                23,
                consent,
                &ProtocolLimits::ABSOLUTE,
                &mut bytes
            )
            .unwrap(),
            bytes.len()
        );
        assert_eq!(
            startup::decode(&bytes, parent(), scope(), 23, &ProtocolLimits::ABSOLUTE).unwrap(),
            consent
        );
    }
}
#[test]
fn every_scope_component_binding_and_consent_are_checked() {
    let mut bytes = [0; startup::RECORD_BYTES];
    startup::encode(
        parent(),
        scope(),
        23,
        true,
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
    )
    .unwrap();
    for i in [
        fr_wire::HEADER_BYTES,
        fr_wire::HEADER_BYTES + 16,
        fr_wire::HEADER_BYTES + 32,
        fr_wire::HEADER_BYTES + 48,
        fr_wire::HEADER_BYTES + 64,
    ] {
        let mut bad = bytes;
        bad[i] ^= 1;
        assert!(startup::decode(&bad, parent(), scope(), 23, &ProtocolLimits::ABSOLUTE).is_err());
    }
    let mut bad = bytes;
    *bad.last_mut().unwrap() = 2;
    assert!(startup::decode(&bad, parent(), scope(), 23, &ProtocolLimits::ABSOLUTE).is_err());
    assert!(startup::decode(&bytes, parent(), scope(), 24, &ProtocolLimits::ABSOLUTE).is_err());
    for channel in [0, parent().id] {
        assert!(
            startup::decode(
                &bytes,
                parent(),
                scope(),
                channel,
                &ProtocolLimits::ABSOLUTE
            )
            .is_err()
        );
    }
}
#[test]
fn no_truncation_suffix_wrong_kind_or_limit_bypass() {
    let mut bytes = [0; startup::RECORD_BYTES];
    startup::encode(
        parent(),
        scope(),
        23,
        true,
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
    )
    .unwrap();
    for end in 0..bytes.len() {
        assert!(
            startup::decode(
                &bytes[..end],
                parent(),
                scope(),
                23,
                &ProtocolLimits::ABSOLUTE
            )
            .is_err()
        );
    }
    let mut extra = bytes.to_vec();
    extra.push(0);
    assert!(startup::decode(&extra, parent(), scope(), 23, &ProtocolLimits::ABSOLUTE).is_err());
    let mut kind = bytes;
    kind[6..8].copy_from_slice(&(fr_wire::Kind::BindingAccepted as u16).to_be_bytes());
    assert!(startup::decode(&kind, parent(), scope(), 23, &ProtocolLimits::ABSOLUTE).is_err());
    let small = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(64),
        ..LimitOverrides::default()
    })
    .unwrap();
    assert!(startup::decode(&bytes, parent(), scope(), 23, &small).is_err());
    assert!(startup::encode(parent(), scope(), 23, true, &small, &mut bytes).is_err());
}
