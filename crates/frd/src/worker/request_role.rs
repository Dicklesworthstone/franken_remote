//! Parent-side admission for an already configured media worker. Runtime,
//! state, source selection, sequence, deadline and capacity checks stay with
//! the original request owner; a rate target grants no additional worker role.
use fr_media::worker::{Kind, Role};

pub(super) fn allowed(role: Role, kind: Kind) -> bool {
    matches!(
        (role, kind),
        (
            Role::Capture,
            Kind::Capture
                | Kind::CaptureIfChanged
                | Kind::CaptureAtRate
                | Kind::CheckMonitor
                | Kind::ReadCursor
        ) | (
            Role::Present,
            Kind::Present | Kind::Decode | Kind::CursorOverlay
        ) | (Role::Audio, Kind::ReadAudio)
            | (Role::Capture | Role::Present, Kind::Poll)
            | (_, Kind::Stop)
    )
}

#[cfg(test)]
mod tests {
    use super::{Kind, Role, allowed};

    #[test]
    fn rate_capture_is_admitted_by_the_parent_only_for_the_capture_role() {
        assert!(allowed(Role::Capture, Kind::CaptureAtRate));
        assert!(!allowed(Role::Present, Kind::CaptureAtRate));
        assert!(!allowed(Role::Audio, Kind::CaptureAtRate));
    }

    #[test]
    fn legacy_operations_keep_their_original_role_boundaries() {
        for (kind, owner) in [
            (Kind::Capture, Role::Capture),
            (Kind::CaptureIfChanged, Role::Capture),
            (Kind::CheckMonitor, Role::Capture),
            (Kind::ReadCursor, Role::Capture),
            (Kind::Present, Role::Present),
            (Kind::Decode, Role::Present),
            (Kind::CursorOverlay, Role::Present),
            (Kind::ReadAudio, Role::Audio),
        ] {
            for role in [Role::Capture, Role::Present, Role::Audio] {
                assert_eq!(allowed(role, kind), role == owner, "{role:?}: {kind:?}");
            }
        }
        for role in [Role::Capture, Role::Present, Role::Audio] {
            assert_eq!(allowed(role, Kind::Poll), role != Role::Audio);
            assert!(allowed(role, Kind::Stop));
        }
    }

    #[test]
    fn bootstrap_and_reply_kinds_cannot_reenter_a_running_worker() {
        for kind in [
            Kind::Configure,
            Kind::ConfigureDecoder,
            Kind::DiscoverCapture,
            Kind::ConfigureCapture,
            Kind::DiscoverMonitors,
            Kind::ConfigureMonitor,
            Kind::ConfigurePresentation,
            Kind::ConfigureFittedPresentation,
            Kind::ConfigureAudio,
            Kind::Ready,
            Kind::Unit,
            Kind::NeedInput,
            Kind::NeedDrain,
            Kind::Presented,
            Kind::Stopped,
            Kind::Refused,
            Kind::Decoded,
            Kind::Unchanged,
            Kind::DecoderReady,
            Kind::CaptureScreens,
            Kind::CaptureReady,
            Kind::CaptureMonitors,
            Kind::MonitorReady,
            Kind::MonitorValid,
            Kind::PresentationReady,
            Kind::FittedPresentationReady,
            Kind::CursorSnapshot,
            Kind::CursorOverlayApplied,
            Kind::AudioReady,
            Kind::AudioPackets,
            Kind::RateUnit,
            Kind::RateUnchanged,
        ] {
            for role in [Role::Capture, Role::Present, Role::Audio] {
                assert!(!allowed(role, kind), "{role:?}: {kind:?}");
            }
        }
    }
}
