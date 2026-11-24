//! Shared native control policy. Text is optional at negotiation and is
//! requested only through the existing host-confirmed capability intersection.
#![forbid(unsafe_code)]

use super::InputResult;
use crate::input::profile::{capabilities as input_capabilities, offer as input_offer};
use fr_core::{input::MAX_COMMITTED_TEXT_BYTES, input_submission::Capabilities};
use fr_wire::negotiation::{Error, Offer};
use frd::session_startup::viewer_events::Error as CaptureError;

pub(super) fn offer(control: bool, clipboard: bool, files: bool) -> Result<Offer, Error> {
    if control {
        // This must be the original reconnect offer. An application callback
        // may remove optional markers later, but cannot add a new one.
        let original = fr_client::native::control_offer_with_auxiliary(clipboard, files, false);
        with_input(original)
    } else {
        Ok(fr_client::native::observation_offer())
    }
}

// Control and three independently selected optional capabilities.
#[allow(clippy::fn_params_excessive_bools)]
pub(super) fn offer_with_download(
    control: bool,
    clipboard: bool,
    upload: bool,
    download: bool,
) -> Result<Offer, Error> {
    if !control || !download {
        return offer(control, clipboard, upload);
    }
    let original = if upload {
        fr_client::native::control_offer_with_duplex(clipboard, false)
    } else {
        fr_client::native::control_offer_with_download(clipboard, false)
    };
    with_input(original)
}

/// Playback is selected only when the shell supplies a local output factory.
/// The original reconnect offer contains its audio scope before optional input.
#[allow(clippy::fn_params_excessive_bools)]
pub(super) fn offer_with_playback(
    control: bool,
    clipboard: bool,
    upload: bool,
    download: bool,
    playback: bool,
) -> Result<Offer, Error> {
    if !playback {
        return offer_with_download(control, clipboard, upload, download);
    }
    if !control {
        return Ok(fr_client::native::observation_offer_with_audio());
    }
    let original = if download && upload {
        fr_client::native::control_offer_with_duplex(clipboard, true)
    } else if download {
        fr_client::native::control_offer_with_download(clipboard, true)
    } else {
        fr_client::native::control_offer_with_auxiliary(clipboard, upload, true)
    };
    with_input(original)
}

fn with_input(original: Offer) -> Result<Offer, Error> {
    // This core's precise producer is the iOS touch interpreter. macOS builds
    // this crate for host tests; its shipping AppKit viewer has a separate
    // fr-native input producer and negotiation path.
    input_offer(original, cfg!(target_os = "ios"))
}

/// The app produces whole committed text but never synthesizes physical repeat.
/// `NativeObserver` meets this intent with the negotiated host profile before
/// sending a control request, so legacy hosts retain physical-key control.
pub(super) fn requested_capabilities() -> Capabilities {
    input_capabilities(cfg!(target_os = "ios"))
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
    use fr_core::input_submission::Capability;
    use fr_wire::control::{TEXT_CAPABILITY, requestable, with_committed_text};
    use frd::session_startup::{host_offer, host_offer_with, with_line_scroll};

    #[test]
    fn ios_precise_scroll_is_selected_only_bilaterally_and_keeps_legacy_control() {
        use fr_wire::control::{PIXEL_SCROLL_CAPABILITY, with_pixel_scroll};
        // Exercise the actual iOS profile on macOS host tests without
        // advertising an iOS producer in a non-iOS production offer.
        let client = input_offer(fr_client::native::control_offer(), true).unwrap();
        assert!(
            client
                .capabilities
                .iter()
                .any(|cap| cap.name == PIXEL_SCROLL_CAPABILITY && !cap.required)
        );
        let legacy = with_line_scroll(host_offer(true));
        let precise = with_pixel_scroll(legacy.clone()).unwrap();
        let mut wrong_version = precise.clone();
        wrong_version
            .capabilities
            .iter_mut()
            .find(|cap| cap.name == PIXEL_SCROLL_CAPABILITY)
            .unwrap()
            .version = 2;
        for (host, wanted) in [(legacy, false), (precise, true), (wrong_version, false)] {
            let selected = host.intersect(&client).unwrap();
            let granted = input_capabilities(true).meet(requestable(&selected.capabilities));
            assert_eq!(granted.contains(Capability::PixelScroll), wanted);
            assert!(granted.contains(Capability::LineScroll));
            assert!(granted.contains(Capability::Keys));
            assert!(!granted.contains(Capability::Repeat));
        }
        let precise_only = with_pixel_scroll(host_offer(true)).unwrap();
        let selected = precise_only.intersect(&client).unwrap();
        let granted = input_capabilities(true).meet(requestable(&selected.capabilities));
        assert!(granted.contains(Capability::PixelScroll));
        assert!(!granted.contains(Capability::LineScroll));
    }

    #[test]
    fn the_platform_offer_and_request_only_advertise_its_actual_scroll_producer() {
        use fr_wire::control::PIXEL_SCROLL_CAPABILITY;
        let offered = offer(true, false, false).unwrap();
        assert_eq!(
            offered
                .capabilities
                .iter()
                .any(|cap| cap.name == PIXEL_SCROLL_CAPABILITY),
            cfg!(target_os = "ios")
        );
        assert_eq!(
            requested_capabilities().contains(Capability::PixelScroll),
            cfg!(target_os = "ios")
        );
        assert!(requested_capabilities().contains(Capability::LineScroll));
    }

    #[test]
    fn selected_download_and_duplex_profiles_keep_text_clipboard_and_input_gates() {
        use fr_wire::{attachment, files};
        for clipboard in [false, true] {
            for upload in [false, true] {
                let offered = offer_with_download(true, clipboard, upload, true).unwrap();
                let has = |name: &str| offered.capabilities.iter().any(|cap| cap.name == name);
                assert!(has(files::DOWNLOAD_CAPABILITY));
                assert!(has(files::CAPABILITY));
                assert!(has(attachment::FILES_CAPABILITY));
                assert!(has(TEXT_CAPABILITY));
                assert_eq!(has(files::CHANNEL_SCOPE_CAPABILITY), upload);
                assert_eq!(has(files::DUPLEX_CAPABILITY), upload);
                assert_eq!(has(attachment::CLIPBOARD_CAPABILITY), clipboard);
                assert!(offered.capabilities.len() <= fr_wire::negotiation::MAX_CAPABILITIES);
                assert!(
                    fr_client::native::control_offer()
                        .capabilities
                        .iter()
                        .filter(|cap| cap.required)
                        .all(|cap| offered.capabilities.contains(cap))
                );
            }
        }
        assert_eq!(
            offer_with_download(false, true, true, true).unwrap(),
            fr_client::native::observation_offer()
        );
    }

    #[test]
    fn original_control_offer_adds_only_optional_text_and_preserves_required_gates() {
        let original = fr_client::native::control_offer();
        let selected = offer(true, false, false).unwrap();
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
            offer(false, true, true).unwrap(),
            fr_client::native::observation_offer()
        );
    }

    #[test]
    fn actual_host_offer_intersection_keeps_legacy_control_and_selects_text_only_bilaterally() {
        let client = offer(true, false, false).unwrap();
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
    fn clipboard_is_optional_and_composes_with_text_without_changing_legacy_input() {
        let client = offer(true, true, false).unwrap();
        let without = offer(true, false, false).unwrap();
        for (name, version) in [
            (
                fr_wire::attachment::CLIPBOARD_CAPABILITY,
                fr_wire::attachment::CLIPBOARD_VERSION,
            ),
            (fr_wire::clipboard::CAPABILITY, fr_wire::clipboard::VERSION),
            (
                fr_wire::clipboard::startup::CAPABILITY,
                fr_wire::clipboard::startup::VERSION,
            ),
        ] {
            assert!(
                client
                    .capabilities
                    .iter()
                    .any(|cap| cap.name == name && cap.version == version && !cap.required)
            );
            assert!(!without.capabilities.iter().any(|cap| cap.name == name));
        }
        assert!(
            client
                .capabilities
                .iter()
                .any(|cap| cap.name == TEXT_CAPABILITY && !cap.required)
        );
        let legacy = with_line_scroll(host_offer(true));
        assert_eq!(
            legacy.intersect(&client).unwrap(),
            legacy.intersect(&without).unwrap()
        );
        let text = with_committed_text(legacy).unwrap();
        assert_eq!(
            text.intersect(&client).unwrap(),
            text.intersect(&without).unwrap()
        );
        assert_eq!(
            offer(false, true, true).unwrap(),
            offer(false, false, false).unwrap()
        );
        assert!(client.capabilities.len() <= fr_wire::negotiation::MAX_CAPABILITIES);
        let bilateral = with_committed_text(with_line_scroll(host_offer_with(true, true)))
            .unwrap()
            .intersect(&client)
            .unwrap();
        for name in [
            TEXT_CAPABILITY,
            fr_wire::attachment::CLIPBOARD_CAPABILITY,
            fr_wire::clipboard::CAPABILITY,
            fr_wire::clipboard::startup::CAPABILITY,
        ] {
            assert!(bilateral.capabilities.iter().any(|cap| cap.name == name));
        }
    }

    #[test]
    fn clipboard_adds_only_optional_markers_selected_only_with_a_clipboard_host() {
        let plain = offer(true, false, false).unwrap();
        let clipboard = offer(true, true, false).unwrap();
        assert_eq!(
            (clipboard.role, &clipboard.versions, &clipboard.limits),
            (plain.role, &plain.versions, &plain.limits)
        );
        assert!(
            plain
                .capabilities
                .iter()
                .all(|cap| clipboard.capabilities.contains(cap))
        );
        // Preserve the upstream exact-set regression, including the optional
        // bulk-record extension used by this separate auxiliary lane.
        let mut added: Vec<_> = clipboard
            .capabilities
            .iter()
            .filter(|cap| !plain.capabilities.contains(cap))
            .collect();
        assert!(added.iter().all(|cap| !cap.required));
        added.sort_by(|a, b| a.name.cmp(&b.name));
        let mut expected = [
            fr_wire::attachment::BULK_RECORD_CAPABILITY,
            fr_wire::attachment::CLIPBOARD_CAPABILITY,
            fr_wire::clipboard::CAPABILITY,
            fr_wire::clipboard::startup::CAPABILITY,
        ];
        expected.sort_unstable();
        assert_eq!(
            added
                .iter()
                .map(|cap| cap.name.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(clipboard.capabilities.len() <= fr_wire::negotiation::MAX_CAPABILITIES);
        for host_clipboard in [true, false] {
            let host = with_committed_text(with_line_scroll(host_offer_with(true, host_clipboard)))
                .unwrap();
            let selected = host.intersect(&clipboard).unwrap();
            let lane = selected
                .capabilities
                .iter()
                .any(|cap| cap.name == fr_wire::clipboard::CAPABILITY);
            assert_eq!(lane, host_clipboard);
            let requested = requested_capabilities().meet(requestable(&selected.capabilities));
            assert!(requested.contains(Capability::Keys));
            assert!(requested.contains(Capability::Text));
        }
    }

    #[test]
    fn explicit_downloads_are_optional_and_compose_with_existing_control_and_uploads() {
        for upload in [false, true] {
            for clipboard in [false, true] {
                assert_eq!(
                    offer_with_download(true, clipboard, upload, false).unwrap(),
                    offer(true, clipboard, upload).unwrap()
                );
                let offered = offer_with_download(true, clipboard, upload, true).unwrap();
                offered.validate().unwrap();
                let has = |name| offered.capabilities.iter().any(|cap| cap.name == name);
                assert!(has(fr_wire::files::DOWNLOAD_CAPABILITY));
                assert_eq!(has(fr_wire::files::DUPLEX_CAPABILITY), upload);
                assert_eq!(has(fr_wire::files::CHANNEL_SCOPE_CAPABILITY), upload);
                assert!(has(fr_wire::control::TEXT_CAPABILITY));
                assert!(
                    offered
                        .capabilities
                        .iter()
                        .filter(|cap| {
                            cap.name == fr_wire::files::DOWNLOAD_CAPABILITY
                                || cap.name == fr_wire::files::DUPLEX_CAPABILITY
                        })
                        .all(|cap| !cap.required)
                );
                let legacy = with_line_scroll(host_offer(true));
                let selected = legacy.intersect(&offered).unwrap();
                assert!(
                    !selected
                        .capabilities
                        .iter()
                        .any(|cap| cap.name == fr_wire::files::DOWNLOAD_CAPABILITY)
                );
                assert!(requestable(&selected.capabilities).contains(Capability::Keys));
            }
        }
        assert_eq!(
            offer_with_download(false, true, true, true).unwrap(),
            fr_client::native::observation_offer()
        );
    }

    #[test]
    fn explicit_files_compose_with_text_and_clipboard_only_on_a_controlling_peer() {
        let original = offer(true, true, false).unwrap();
        let upload = offer(true, true, true).unwrap();
        assert!(upload.capabilities.len() <= fr_wire::negotiation::MAX_CAPABILITIES);
        let omitted: Vec<_> = original
            .capabilities
            .iter()
            .filter(|cap| !upload.capabilities.contains(cap))
            .map(|cap| (cap.name.as_str(), cap.required))
            .collect();
        if cfg!(target_os = "ios") {
            // The full iOS profile fills all 24 slots. Exactly the optional
            // diagnostic yields; input, clipboard and ATP boundaries stay.
            assert_eq!(
                omitted,
                vec![(fr_wire::quality_decision::CAPABILITY, false)]
            );
        } else {
            assert_eq!(omitted, [] as [(&str, bool); 0]);
        }
        let file_names = [
            fr_wire::attachment::FILES_CAPABILITY,
            fr_wire::files::CAPABILITY,
            fr_wire::files::CHANNEL_SCOPE_CAPABILITY,
        ];
        for name in file_names {
            assert!(
                upload
                    .capabilities
                    .iter()
                    .any(|cap| cap.name == name && !cap.required)
            );
            assert!(!original.capabilities.iter().any(|cap| cap.name == name));
        }
        for enabled in [true, false] {
            let host = with_committed_text(with_line_scroll(
                frd::session_startup::host_offer_with_files(true, true, false, enabled),
            ))
            .unwrap();
            let selected = host.intersect(&upload).unwrap();
            for name in file_names {
                assert_eq!(
                    selected.capabilities.iter().any(|cap| cap.name == name),
                    enabled
                );
            }
            assert!(
                selected
                    .capabilities
                    .iter()
                    .any(|cap| cap.name == TEXT_CAPABILITY)
            );
            assert!(
                selected
                    .capabilities
                    .iter()
                    .any(|cap| cap.name == fr_wire::clipboard::CAPABILITY)
            );
        }
        assert_eq!(
            offer(false, true, true).unwrap(),
            fr_client::native::observation_offer()
        );
    }

    #[test]
    #[cfg(not(target_os = "ios"))]
    fn windows_playback_fits_text_clipboard_and_both_file_directions() {
        let client = offer_with_playback(true, true, true, true, true).unwrap();
        client.validate().unwrap();
        let selected = client.select().unwrap();
        assert!(fr_wire::audio_control::downlink_selected(&selected));
        assert!(fr_wire::files::duplex_selected(&selected));
        assert!(requestable(&selected.capabilities).contains(Capability::Text));
        assert!(
            selected
                .capabilities
                .iter()
                .any(|cap| cap.name == fr_wire::clipboard::CAPABILITY)
        );
        assert!(client.capabilities.len() <= fr_wire::negotiation::MAX_CAPABILITIES);

        // A legacy host still agrees to the same usable input/file profile;
        // the optional local output never invents remote capture authority.
        let legacy = offer_with_playback(true, true, true, true, false).unwrap();
        let intersection = legacy.intersect(&client).unwrap().select().unwrap();
        assert!(!fr_wire::audio_control::downlink_selected(&intersection));
        assert!(fr_wire::files::duplex_selected(&intersection));
        assert!(requestable(&intersection.capabilities).contains(Capability::Text));
    }

    #[test]
    fn explicit_observing_playback_does_not_request_control_or_auxiliary_authority() {
        let audio = offer_with_playback(false, true, true, true, true).unwrap();
        assert_eq!(audio, fr_client::native::observation_offer_with_audio());
        let plain = offer_with_playback(false, true, true, true, false).unwrap();
        assert_eq!(plain, fr_client::native::observation_offer());
        let legacy = plain.intersect(&audio).unwrap().select().unwrap();
        assert!(
            !legacy
                .capabilities
                .iter()
                .any(|cap| cap.name == fr_wire::audio::CAPABILITY)
        );
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
