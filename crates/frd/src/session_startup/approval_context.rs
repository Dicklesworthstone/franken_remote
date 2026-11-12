//! Original admitted identity and exact negotiated scope, for local UI only.
use super::{Error, Peer, Selection};
use fr_core::indicator_process::approval::{Context, Features};
use std::sync::atomic::{AtomicU64, Ordering};

// A display reference only. Never derived from a transport, session binding,
// approval bearer or native IPC epoch; exhaustion refuses without wrapping.
static LOCAL_REQUEST: AtomicU64 = AtomicU64::new(0);

pub(super) struct Attribution {
    node_id: String,
    dns_name: Option<String>,
    features: Features,
    local_request: u64,
}
impl Attribution {
    pub(super) fn new(peer: &Peer, selected: &Selection) -> Result<Self, Error> {
        let (node_id, dns_name) = match peer {
            Peer::Tailnet(owner) => {
                let identity = owner
                    .lease()
                    .approval_identity()
                    .map_err(Error::Admission)?;
                (
                    identity.node_id().to_owned(),
                    identity.dns_name().map(str::to_owned),
                )
            }
            #[cfg(test)]
            Peer::Fixture { .. } => ("fixture-node".into(), Some("fixture.example".into())),
        };
        let local_request = LOCAL_REQUEST
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| Error::InvalidConfiguration)?
            .checked_add(1)
            .ok_or(Error::InvalidConfiguration)?;
        Ok(Self {
            node_id,
            dns_name,
            features: features(selected),
            local_request,
        })
    }
    pub(super) fn context(&self) -> Result<Context, Error> {
        Context::new(
            &self.node_id,
            self.dns_name.as_deref().filter(|name| !name.is_empty()),
            self.features,
            self.local_request,
        )
        .map_err(|_| Error::InvalidConfiguration)
    }
}
fn features(selected: &Selection) -> Features {
    [
        (
            fr_wire::audio_control::downlink_selected(selected),
            Features::PLAYBACK,
        ),
        (
            super::clipboard::selected(selected).is_ok(),
            Features::CLIPBOARD,
        ),
        (
            super::native_control::files_lane(selected).is_ok(),
            Features::FILE_SEND,
        ),
        (
            super::native_control::file_download_lane(selected).is_ok(),
            Features::FILE_RECEIVE,
        ),
        (
            fr_wire::audio_up::uplink_selected(selected),
            Features::MICROPHONE,
        ),
    ]
    .into_iter()
    .filter(|(selected, _)| *selected)
    .fold(Features::NONE, |features, (_, feature)| {
        features.with(feature)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_wire::negotiation::Role;

    #[test]
    fn empty_daemon_alias_is_absent_but_unsafe_nonempty_alias_still_refuses() {
        let mut attribution = Attribution {
            node_id: "nOriginal".into(),
            dns_name: Some(String::new()),
            features: Features::NONE,
            local_request: 1,
        };
        let hello = fr_core::indicator_process::Frame {
            kind: fr_core::indicator_process::Kind::ApproveViewDetails,
            sequence: 1,
            epoch: 7,
        };
        let details = attribution.context().unwrap().details(hello).unwrap();
        assert!(details.contains("Verified node: nOriginal"));
        assert!(details.contains("Daemon name: (unavailable)"));
        attribution.dns_name = Some("\n".into());
        assert_eq!(attribution.context(), Err(Error::InvalidConfiguration));
    }

    #[test]
    fn approval_discloses_effective_negotiated_scope_not_offered_names() {
        let mut offer = super::super::host_offer_with_duplex(true, true, true);
        offer.role = Role::RequestControl;
        let mut selected = fr_wire::audio_up::with_microphone(offer)
            .unwrap()
            .select()
            .unwrap();
        let expected = Features::PLAYBACK
            .with(Features::CLIPBOARD)
            .with(Features::FILE_SEND)
            .with(Features::FILE_RECEIVE)
            .with(Features::MICROPHONE);
        assert_eq!(features(&selected), expected);
        // A read-only request cannot display control-owned effects as allowed,
        // even if its optional capability list contains those names.
        selected.role = Role::Observe;
        assert_eq!(features(&selected), Features::PLAYBACK);
        selected.role = Role::RequestControl;
        selected
            .capabilities
            .retain(|cap| cap.name != fr_wire::audio_control::CAPABILITY);
        assert!(!features(&selected).contains(Features::PLAYBACK));
        assert!(features(&selected).contains(Features::CLIPBOARD));
    }
}
