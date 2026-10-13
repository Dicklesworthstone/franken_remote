//! Additional identity and exhaustion fences for reference recovery.
use super::*;
use fr_core::ids::{
    CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, OsSessionId,
    RemoteSessionId, ViewportMappingGeneration,
};

fn original() -> Binding {
    Binding {
        parent: ControlBinding {
            id: 8,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(13),
        },
        display: 9,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}

fn replacement(previous: Binding, channel: u32) -> Binding {
    Binding {
        parent: ControlBinding {
            id: channel,
            ..previous.parent
        },
        recovery: previous.recovery.next().unwrap(),
        ..previous
    }
}

#[test]
fn consecutive_recoveries_advance_the_existing_identity_exactly_once() {
    let first = original();
    let second = replacement(first, 20);
    let third = replacement(second, 30);
    assert!(recovered_successor(first, second));
    assert!(recovered_successor(second, third));
    assert_eq!(first.recovery, RecoveryGeneration::INITIAL);
}

#[test]
fn configuration_channel_cannot_be_zero_or_reused() {
    let first = original();
    for channel in [0, first.parent.id] {
        assert!(
            !recovered_successor(first, replacement(first, channel)),
            "configuration channel {channel} must not be installed"
        );
    }
}

#[test]
fn recovery_cannot_replay_skip_or_roll_back_a_generation() {
    let mut first = original();
    first.recovery = RecoveryGeneration::from_raw(7);
    for generation in [0, 6, 7, 9, u64::MAX] {
        let mut next = replacement(first, 20);
        next.recovery = RecoveryGeneration::from_raw(generation);
        assert!(
            !recovered_successor(first, next),
            "recovery generation {generation} is not the exact successor"
        );
    }
}

#[test]
fn recovery_exhaustion_never_wraps() {
    let mut first = original();
    first.recovery = RecoveryGeneration::from_raw(u64::MAX);
    let mut next = first;
    next.parent.id = 20;
    next.recovery = RecoveryGeneration::INITIAL;
    assert!(!recovered_successor(first, next));
}

#[test]
fn every_authority_display_and_mapping_field_is_fenced() {
    let first = original();
    let next = replacement(first, 20);
    let mut candidates = [next; 7];
    candidates[0].parent.host_boot = HostBootId::from_raw(999);
    candidates[1].parent.os_session = OsSessionId::from_raw(999);
    candidates[2].parent.remote_session = RemoteSessionId::from_raw(999);
    candidates[3].display += 1;
    candidates[4].geometry = next.geometry.next().unwrap();
    candidates[5].configuration = next.configuration.next().unwrap();
    candidates[6].viewport = next.viewport.next().unwrap();
    for (field, candidate) in candidates.into_iter().enumerate() {
        assert!(
            !recovered_successor(first, candidate),
            "identity field {field} changed during a reference-only recovery"
        );
    }
}
