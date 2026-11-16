//! Downward-only pruning of independently spent native file consent.
#![forbid(unsafe_code)]

use fr_wire::negotiation::Offer;

/// A fresh attempt may keep either original direction. Download-only omits
/// the legacy upload scope; otherwise it is not a valid download selection.
/// An existing channel is never rebound or converted by this function.
pub(super) fn retain(original: &Offer, upload: bool, download: bool) -> Result<Offer, ()> {
    let keep = |name: &str| match name {
        fr_wire::attachment::FILES_CAPABILITY | fr_wire::files::CAPABILITY => upload || download,
        fr_wire::files::CHANNEL_SCOPE_CAPABILITY => upload,
        fr_wire::files::DOWNLOAD_CAPABILITY => download,
        fr_wire::files::DUPLEX_CAPABILITY => upload && download,
        _ => true,
    };
    if original
        .capabilities
        .iter()
        .any(|cap| cap.required && !keep(&cap.name))
    {
        return Err(());
    }
    let mut offer = original.clone();
    offer.capabilities.retain(|cap| keep(&cap.name));
    Ok(offer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_wire::files;
    use frd::session_startup::{
        host_offer_with_download, host_offer_with_duplex, host_offer_with_files,
    };

    fn has(offer: &Offer, name: &str) -> bool {
        offer.capabilities.iter().any(|cap| cap.name == name)
    }

    fn original() -> Offer {
        fr_wire::control::with_committed_text(fr_client::native::control_offer_with_duplex(
            true, false,
        ))
        .unwrap()
    }

    #[test]
    fn each_direction_retains_its_original_scope_and_shared_capabilities() {
        let original = original();
        for (upload, download) in [(true, true), (true, false), (false, true), (false, false)] {
            let offered = retain(&original, upload, download).unwrap();
            assert_eq!(
                has(&offered, fr_wire::attachment::FILES_CAPABILITY),
                upload || download
            );
            assert_eq!(has(&offered, files::CAPABILITY), upload || download);
            assert_eq!(has(&offered, files::CHANNEL_SCOPE_CAPABILITY), upload);
            assert_eq!(has(&offered, files::DOWNLOAD_CAPABILITY), download);
            assert_eq!(has(&offered, files::DUPLEX_CAPABILITY), upload && download);
            assert!(
                offered
                    .capabilities
                    .iter()
                    .all(|cap| original.capabilities.contains(cap))
            );
            assert!(
                original
                    .capabilities
                    .iter()
                    .filter(|cap| cap.required)
                    .all(|cap| offered.capabilities.contains(cap))
            );
            assert!(offered.capabilities.len() <= fr_wire::negotiation::MAX_CAPABILITIES);
        }
    }

    #[test]
    fn pruned_duplex_offers_meet_legacy_and_duplex_hosts_without_crossing_directions() {
        let original = original();
        let hosts = [
            host_offer_with_files(true, true, false, true),
            host_offer_with_download(true, true, false, true),
            host_offer_with_duplex(true, true, false),
        ];
        for (upload, download) in [(true, true), (true, false), (false, true), (false, false)] {
            let offered = retain(&original, upload, download).unwrap();
            for (host_kind, host) in hosts.iter().enumerate() {
                let selected = host.intersect(&offered).unwrap();
                assert_eq!(
                    files::duplex_selected(&selected),
                    host_kind == 2 && upload && download,
                );
                let common = selected
                    .capabilities
                    .iter()
                    .any(|cap| cap.name == files::CAPABILITY);
                let scope = selected
                    .capabilities
                    .iter()
                    .any(|cap| cap.name == files::CHANNEL_SCOPE_CAPABILITY);
                let receives = selected
                    .capabilities
                    .iter()
                    .any(|cap| cap.name == files::DOWNLOAD_CAPABILITY);
                assert_eq!(scope && common, upload && host_kind != 1);
                assert_eq!(receives && common, download && host_kind != 0);
            }
        }
    }

    #[test]
    fn pruning_never_adds_a_direction_or_removes_a_required_capability() {
        let upload = fr_client::native::control_offer_with_auxiliary(true, true, false);
        assert!(!has(
            &retain(&upload, false, true).unwrap(),
            files::DOWNLOAD_CAPABILITY
        ));
        let mut required = original();
        required
            .capabilities
            .iter_mut()
            .find(|cap| cap.name == files::DOWNLOAD_CAPABILITY)
            .unwrap()
            .required = true;
        assert!(retain(&required, true, false).is_err());
        assert!(retain(&required, false, true).is_ok());
        let spent = retain(&required, true, true).unwrap();
        assert_eq!(spent, required);
    }
}
