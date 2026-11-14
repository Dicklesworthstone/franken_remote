//! Shared native control policy. Text is optional at negotiation and is
//! requested only through the existing host-confirmed capability intersection.
#![forbid(unsafe_code)]

use super::InputResult;
use fr_core::{
    input::MAX_COMMITTED_TEXT_BYTES,
    input_submission::{Capabilities, Capability},
};
use fr_wire::negotiation::{Error, Offer};
use frd::session_startup::viewer_events::Error as CaptureError;

pub(super) fn offer(control: bool) -> Result<Offer, Error> {
    if control {
        // This must be the original reconnect offer. An application callback
        // may remove optional markers later, but cannot add a new one.
        fr_client::native::control_offer_with_text()
    } else {
        Ok(fr_client::native::observation_offer())
    }
}

/// The app produces whole committed text but never synthesizes physical repeat.
/// `NativeObserver` meets this intent with the negotiated host profile before
/// sending a control request, so legacy hosts retain physical-key control.
pub(super) fn requested_capabilities() -> Capabilities {
    Capabilities::default()
        .with(Capability::Keys)
        .with(Capability::Absolute)
        .with(Capability::Buttons)
        .with(Capability::LineScroll)
        .with(Capability::Text)
}

/// The C string boundary cannot represent an embedded NUL. Reject the whole
/// value before capture; no prefix or separately retried chunks are accepted.
pub(super) fn valid_text(text: &str) -> bool {
    !text.is_empty() && text.len() <= MAX_COMMITTED_TEXT_BYTES && !text.as_bytes().contains(&0)
}

/// Source explicitly keeps keys alive after these whole-commit refusals.
/// The UI reports the refusal and discards the value; `Busy` is never a license
/// to capture the same completed text again under a new time or context.
pub(super) fn text_result(result: Result<(), CaptureError>) -> InputResult {
    match result {
        Ok(()) => InputResult::Sent,
        Err(CaptureError::UnsupportedText) => InputResult::Unsupported,
        Err(CaptureError::StaleTextContext | CaptureError::TextNotReady) => InputResult::Busy,
        Err(_) => InputResult::ControlLost,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_wire::control::{TEXT_CAPABILITY, requestable, with_committed_text};
    use frd::session_startup::{host_offer, with_line_scroll};

    #[test]
    fn original_control_offer_adds_only_optional_text_and_preserves_required_gates() {
        let original = fr_client::native::control_offer();
        let selected = offer(true).unwrap();
        assert_eq!(selected.role, original.role);
        assert_eq!(selected.versions, original.versions);
        assert_eq!(selected.profile, original.profile);
        assert_eq!(selected.profile_version, original.profile_version);
        assert_eq!(selected.limits, original.limits);
        assert!(
            original
                .capabilities
                .iter()
                .filter(|cap| cap.required)
                .all(|cap| selected.capabilities.contains(cap))
        );
        assert!(
            selected
                .capabilities
                .iter()
                .any(|cap| cap.name == TEXT_CAPABILITY && cap.version == 1 && !cap.required)
        );
        assert!(selected.capabilities.len() <= fr_wire::negotiation::MAX_CAPABILITIES);
        assert_eq!(
            offer(false).unwrap(),
            fr_client::native::observation_offer()
        );
    }

    #[test]
    fn actual_host_offer_intersection_keeps_legacy_control_and_selects_text_only_bilaterally() {
        let client = offer(true).unwrap();
        let legacy = with_line_scroll(host_offer(true));
        let text = with_committed_text(legacy.clone()).unwrap();
        let mut wrong_version = text.clone();
        wrong_version
            .capabilities
            .iter_mut()
            .find(|cap| cap.name == TEXT_CAPABILITY)
            .unwrap()
            .version = 2;
        for (host, expected_text) in [(legacy, false), (text, true), (wrong_version, false)] {
            let selected = host.intersect(&client).unwrap();
            let requested = requested_capabilities().meet(requestable(&selected.capabilities));
            assert_eq!(requested.contains(Capability::Text), expected_text);
            for capability in [
                Capability::Keys,
                Capability::Absolute,
                Capability::Buttons,
                Capability::LineScroll,
            ] {
                assert!(requested.contains(capability), "{capability:?}");
            }
            assert!(!requested.contains(Capability::Repeat));
            assert!(!requested.contains(Capability::PixelScroll));
            assert!(!requested.contains(Capability::Relative));
        }
        let no_wheel = host_offer(true).intersect(&client).unwrap();
        let requested = requested_capabilities().meet(requestable(&no_wheel.capabilities));
        assert!(requested.contains(Capability::Keys));
        assert!(!requested.contains(Capability::Text));
        assert!(!requested.contains(Capability::LineScroll));
    }

    #[test]
    fn text_payload_bound_is_whole_utf8_and_rejects_c_string_truncation() {
        assert!(valid_text("ASCII é e\u{301} 漢🙂\r\n\t"));
        let exact = "🙂".repeat(MAX_COMMITTED_TEXT_BYTES / 4);
        assert_eq!(exact.len(), MAX_COMMITTED_TEXT_BYTES);
        assert!(valid_text(&exact));
        assert!(!valid_text(&format!("{exact}a")));
        assert!(!valid_text(""));
        assert!(!valid_text("before\0after"));
    }

    #[test]
    fn whole_commit_refusal_preserves_keys_but_source_failures_still_end_control() {
        assert_eq!(text_result(Ok(())), InputResult::Sent);
        assert_eq!(
            text_result(Err(CaptureError::UnsupportedText)),
            InputResult::Unsupported
        );
        for error in [CaptureError::StaleTextContext, CaptureError::TextNotReady] {
            assert_eq!(text_result(Err(error)), InputResult::Busy);
        }
        for error in [
            CaptureError::Closed,
            CaptureError::Clock,
            CaptureError::Expired,
            CaptureError::Overflow,
            CaptureError::Unavailable,
        ] {
            assert_eq!(text_result(Err(error)), InputResult::ControlLost);
        }
    }
}
