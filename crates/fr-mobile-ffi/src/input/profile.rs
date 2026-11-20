//! Optional input negotiation follows the concrete shell's producer.
//! These same pure helpers run before session startup and in portable tests.
#![forbid(unsafe_code)]

use fr_core::input_submission::{Capabilities, Capability};
use fr_wire::negotiation::{Error, Offer};

pub(crate) fn offer(original: Offer, precise_scroll: bool) -> Result<Offer, Error> {
    let offered = fr_wire::control::with_committed_text(original)?;
    if precise_scroll {
        fr_wire::control::with_pixel_scroll(offered)
    } else {
        Ok(offered)
    }
}

pub(crate) fn capabilities(precise_scroll: bool) -> Capabilities {
    let requested = Capabilities::default()
        .with(Capability::Keys)
        .with(Capability::Absolute)
        .with(Capability::Buttons)
        .with(Capability::LineScroll)
        .with(Capability::Text);
    if precise_scroll {
        requested.with(Capability::PixelScroll)
    } else {
        requested
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_wire::control::{
        LINE_SCROLL_CAPABILITY, PIXEL_SCROLL_CAPABILITY, TEXT_CAPABILITY, requestable,
    };

    #[test]
    fn precise_scroll_is_advertised_and_requested_only_for_its_producer() {
        for precise in [false, true] {
            let offered = offer(fr_client::native::control_offer(), precise).unwrap();
            let has = |name: &str| offered.capabilities.iter().any(|cap| cap.name == name);
            assert_eq!(has(PIXEL_SCROLL_CAPABILITY), precise);
            assert!(has(LINE_SCROLL_CAPABILITY));
            assert!(has(TEXT_CAPABILITY));
            let requested = capabilities(precise);
            assert_eq!(requested.contains(Capability::PixelScroll), precise);
            assert!(requested.contains(Capability::LineScroll));
            assert!(!requested.contains(Capability::Repeat));
            assert!(!requested.contains(Capability::Relative));
        }
    }

    #[test]
    fn input_profiles_preserve_each_file_direction_and_required_boundary() {
        for precise in [false, true] {
            for clipboard in [false, true] {
                for original in [
                    fr_client::native::control_offer_with_auxiliary(clipboard, false, false),
                    fr_client::native::control_offer_with_auxiliary(clipboard, true, false),
                    fr_client::native::control_offer_with_download(clipboard, false),
                    fr_client::native::control_offer_with_duplex(clipboard, false),
                ] {
                    let offered = offer(original.clone(), precise).unwrap();
                    let has = |name: &str| offered.capabilities.iter().any(|cap| cap.name == name);
                    assert_eq!(has(PIXEL_SCROLL_CAPABILITY), precise);
                    assert!(has(TEXT_CAPABILITY));
                    assert!(offered.capabilities.len() <= fr_wire::negotiation::MAX_CAPABILITIES);
                    assert!(
                        original
                            .capabilities
                            .iter()
                            .filter(|cap| cap.required)
                            .all(|cap| offered.capabilities.contains(cap))
                    );
                    for boundary in [
                        fr_wire::attachment::FILES_CAPABILITY,
                        fr_wire::files::CAPABILITY,
                        fr_wire::files::DOWNLOAD_CAPABILITY,
                        fr_wire::files::CHANNEL_SCOPE_CAPABILITY,
                        fr_wire::files::DUPLEX_CAPABILITY,
                        fr_wire::attachment::CLIPBOARD_CAPABILITY,
                    ] {
                        assert_eq!(
                            has(boundary),
                            original.capabilities.iter().any(|cap| cap.name == boundary),
                            "{boundary}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn real_wire_intersections_keep_legacy_and_precise_scroll_independent() {
        for producer in [false, true] {
            let client = offer(fr_client::native::control_offer(), producer).unwrap();
            for lines in [false, true] {
                for precise_version in [None, Some(1), Some(2)] {
                    // Explicit peer profiles exercise the actual wire
                    // intersection; this is not a native-host qualification.
                    let mut peer = offer(
                        fr_client::native::control_offer(),
                        precise_version.is_some(),
                    )
                    .unwrap();
                    if !lines {
                        peer.capabilities
                            .retain(|cap| cap.name != LINE_SCROLL_CAPABILITY);
                    }
                    if let Some(version) = precise_version {
                        peer.capabilities
                            .iter_mut()
                            .find(|cap| cap.name == PIXEL_SCROLL_CAPABILITY)
                            .unwrap()
                            .version = version;
                    }
                    let selected = peer.intersect(&client).unwrap();
                    let requested =
                        capabilities(producer).meet(requestable(&selected.capabilities));
                    assert_eq!(requested.contains(Capability::LineScroll), lines);
                    assert_eq!(
                        requested.contains(Capability::PixelScroll),
                        producer && precise_version == Some(1)
                    );
                    assert!(requested.contains(Capability::Keys));
                    assert!(!requested.contains(Capability::Repeat));
                }
            }
        }
    }
}
