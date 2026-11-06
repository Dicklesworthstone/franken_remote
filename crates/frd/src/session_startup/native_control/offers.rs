//! Stateless native host offers; capability selection never grants authority.
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    attachment, clock, control, decoder,
    negotiation::{self, Capability, Offer, Role},
    presented, receiver_metrics,
};

/// The native host's offer. The four observation bootstrap capabilities are
/// always required. With `control`, the four explicit control boundaries are
/// offered OPTIONAL: an observer's intersection drops them, while a
/// controller's own required copies select them. Offering is never a grant,
/// and without `control` a controller's offer fails as `RequiredCapability`.
/// The role of the host's own offer is not negotiated (the intersection
/// carries the peer's role).
pub fn host_offer(control: bool) -> Offer {
    host_offer_with(control, false)
}
/// The three boundaries of the optional text clipboard (its attachment role,
/// record codec and bilateral readiness). Offered OPTIONAL by both sides, so
/// either side's absence drops them and control still negotiates.
pub const CLIPBOARD_CAPABILITIES: [(&str, u16); 3] = [
    (
        attachment::CLIPBOARD_CAPABILITY,
        attachment::CLIPBOARD_VERSION,
    ),
    (fr_wire::clipboard::CAPABILITY, fr_wire::clipboard::VERSION),
    (
        fr_wire::clipboard::startup::CAPABILITY,
        fr_wire::clipboard::startup::VERSION,
    ),
];
/// `host_offer`, plus (only with `control` AND the operator's local clipboard
/// enable) the optional clipboard boundaries. Selecting them is never consent:
/// the host still needs its own configured owner, and the lane attaches only
/// under the controller's active input attachment.
pub fn host_offer_with(control: bool, clipboard: bool) -> Offer {
    host_offer_with_audio(control, clipboard, false)
}
/// [`host_offer_with`] plus, only when the operator LOCALLY enabled playback
/// capture, the optional audio-down capability. Offering it is not an
/// approval: audio still waits for this session's admitted observation, and a
/// viewer that does not offer it receives no audio record.
pub fn host_offer_with_audio(control: bool, clipboard: bool, audio: bool) -> Offer {
    host_offer_with_files(control, clipboard, audio, false)
}
/// The three boundaries of the optional viewer-to-host drop lane: its
/// attachment role, the ATP full-object envelope and channel-derived scope
/// (no out-of-band handle, no host path). Offered OPTIONAL by both sides.
pub const FILE_CAPABILITIES: [(&str, u16); 3] = [
    (attachment::FILES_CAPABILITY, attachment::FILES_VERSION),
    (fr_wire::files::CAPABILITY, fr_wire::files::VERSION),
    (
        fr_wire::files::CHANNEL_SCOPE_CAPABILITY,
        fr_wire::files::CHANNEL_SCOPE_VERSION,
    ),
];
/// [`host_offer_with_audio`] plus, only with `control` AND the operator's
/// local `--files` drop directory, the optional file boundaries. Selecting
/// them is not a transfer: the lane is offered only on the controller's own
/// session after its grant, and attaches only under its active input lease.
// Independent operator opt-ins, each one plain capability switch.
#[allow(clippy::fn_params_excessive_bools)]
pub fn host_offer_with_files(control: bool, clipboard: bool, audio: bool, files: bool) -> Offer {
    let observation = [
        (fr_wire::display::CAPABILITY, 1),
        (decoder::CAPABILITY, 1),
        (attachment::CAPABILITY, 1),
        (attachment::DELIVERY_CAPABILITY, 1),
    ];
    let controlling = [
        (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
        (control::GRANT_CAPABILITY, 1),
        (clock::CAPABILITY, clock::VERSION),
        (presented::CAPABILITY, presented::VERSION),
    ];
    let mut capabilities: Vec<_> = observation
        .iter()
        .map(|&(name, version)| (name, version, true))
        // Each endpoint advertises its existing role-specific ceiling in the
        // attachment exchange. This can only narrow the host's reserved grant.
        .chain([(
            attachment::credit::CAPABILITY,
            attachment::credit::VERSION,
            false,
        )])
        .chain(
            controlling
                .iter()
                .filter(|_| control)
                .map(|&(name, version)| (name, version, false)),
        )
        // Extended tickets report the actual authority cap, then optionally
        // the successfully applied presentation sequence for causal resumption.
        .chain(
            [
                (
                    fr_wire::input_ticket::AUTHORITY_CAPABILITY,
                    fr_wire::input_ticket::AUTHORITY_VERSION,
                    false,
                ),
                (
                    fr_wire::input_ticket::READINESS_CAPABILITY,
                    fr_wire::input_ticket::READINESS_VERSION,
                    false,
                ),
            ]
            .into_iter()
            .filter(|_| control),
        )
        // Remote-cursor forwarding is optional in both modes: an older viewer
        // omits it and receives no cursor records.
        .chain([(fr_wire::cursor::CAPABILITY, fr_wire::cursor::VERSION, false)])
        // Reference recovery: a viewer whose reference chain broke reports it
        // and gets a fresh recovery IDR on new channels instead of a full
        // reconnect. A controller's input stays suspended meanwhile and
        // resumes on the recovered generation (see running/publisher.rs).
        .chain([(
            fr_wire::recovery_request::CAPABILITY,
            fr_wire::recovery_request::VERSION,
            false,
        )])
        .chain(
            CLIPBOARD_CAPABILITIES
                .iter()
                .filter(|_| control && clipboard)
                .map(|&(name, version)| (name, version, false)),
        )
        .chain(
            [(fr_wire::audio::CAPABILITY, fr_wire::audio::VERSION, false)]
                .into_iter()
                .filter(|_| audio),
        )
        // Legacy hosts offer audio-down even when their control path cannot
        // use it. The extra OPTIONAL boundary prevents an attachment deadlock
        // with those hosts without making audio mandatory for control.
        .chain(
            [(
                fr_wire::audio_control::CAPABILITY,
                fr_wire::audio_control::VERSION,
                false,
            )]
            .into_iter()
            .filter(|_| control && audio),
        )
        .chain(
            FILE_CAPABILITIES
                .iter()
                .filter(|_| control && files)
                .map(|&(name, version)| (name, version, false)),
        )
        // A record-size opt-in, not another clipboard/file permission. Legacy
        // peers omit it and keep their critical-sized bulk record allowance.
        .chain(
            [(
                attachment::BULK_RECORD_CAPABILITY,
                attachment::BULK_RECORD_VERSION,
                false,
            )]
            .into_iter()
            .filter(|_| control && (clipboard || files)),
        )
        .map(|(name, version, required)| Capability {
            name: name.into(),
            version,
            required,
        })
        .collect();
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: Role::Observe,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities,
    }
}

/// The independently selected host source, using the Files pair in the reverse
/// direction. It never advertises a legacy upload scope without a local drop
/// directory. An older viewer can still control the desktop, but cannot request
/// this direction without the explicit download capability.
#[allow(clippy::fn_params_excessive_bools)]
pub fn host_offer_with_download(
    control: bool,
    clipboard: bool,
    audio: bool,
    download: bool,
) -> Offer {
    let mut offer = host_offer_with_files(control, clipboard, audio, download);
    for cap in &mut offer.capabilities {
        if cap.name == fr_wire::files::CHANNEL_SCOPE_CAPABILITY {
            cap.name = fr_wire::files::DOWNLOAD_CAPABILITY.into();
            cap.version = fr_wire::files::DOWNLOAD_VERSION;
        }
    }
    offer.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    offer
}

/// Both an explicitly configured host drop directory and a finite host source
/// selection. Each direction retains an independent one-use file attachment;
/// optional negotiation permits a legacy peer's original single direction.
#[allow(clippy::fn_params_excessive_bools)]
pub fn host_offer_with_duplex(control: bool, clipboard: bool, audio: bool) -> Offer {
    let mut offer = host_offer_with_files(control, clipboard, audio, true);
    if control {
        for (name, version) in [
            (
                fr_wire::files::DOWNLOAD_CAPABILITY,
                fr_wire::files::DOWNLOAD_VERSION,
            ),
            (
                fr_wire::files::DUPLEX_CAPABILITY,
                fr_wire::files::DUPLEX_VERSION,
            ),
        ] {
            offer.capabilities.push(Capability {
                name: name.into(),
                version,
                required: false,
            });
        }
    }
    bound_duplex_offer(&mut offer);
    offer.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    offer
}
fn bound_duplex_offer(offer: &mut Offer) {
    if offer.capabilities.len() > fr_wire::negotiation::MAX_CAPABILITIES
        && offer
            .capabilities
            .iter()
            .any(|cap| cap.name == fr_wire::files::DUPLEX_CAPABILITY)
    {
        // Legacy fixed attachment allowances stay bounded and validated. This
        // optional omission keeps the original negotiation count ceiling, all
        // control/privacy boundaries, cursor, wheel and recovery capabilities.
        offer
            .capabilities
            .retain(|cap| cap.name != attachment::credit::CAPABILITY);
    }
}

/// Only for a control-capable host whose probed input executor can scroll by
/// lines: the optional `native-input-line-scroll` boundary. A controller asks
/// for `LineScroll` only when both peers select it; without it the wheel alone
/// is unavailable and every other granted operation still works.
#[must_use]
pub fn with_line_scroll(mut offer: Offer) -> Offer {
    offer.capabilities.push(Capability {
        name: control::LINE_SCROLL_CAPABILITY.into(),
        version: control::LINE_SCROLL_VERSION,
        required: false,
    });
    bound_duplex_offer(&mut offer);
    offer.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    offer
}

/// Enable the implemented, solicited decoder-load feedback for an explicitly
/// adaptive host. Selection is optional and reports neither attachment credit
/// nor presentation or input authority. An older viewer may omit it entirely.
///
/// At the capability ceiling, prefer feedback over the OPTIONAL bulk-record
/// sizing extension: that peer keeps the existing bounded legacy record size.
/// No required capability or numeric limit is removed or widened. An offer
/// that still cannot fit, or already has an incompatible metrics boundary, is
/// refused instead of silently losing the requested feedback.
pub fn with_receiver_metrics(mut offer: Offer) -> Result<Offer, negotiation::Error> {
    offer.validate()?;
    if let Some(existing) = offer
        .capabilities
        .iter()
        .find(|cap| cap.name == receiver_metrics::CAPABILITY)
    {
        return if existing.version == receiver_metrics::VERSION && !existing.required {
            Ok(offer)
        } else {
            Err(negotiation::Error::RequiredCapability)
        };
    }
    if offer.capabilities.len() == negotiation::MAX_CAPABILITIES {
        offer
            .capabilities
            .retain(|cap| cap.name != attachment::BULK_RECORD_CAPABILITY || cap.required);
    }
    if offer.capabilities.len() >= negotiation::MAX_CAPABILITIES {
        return Err(negotiation::Error::Invalid);
    }
    offer
        .capabilities
        .try_reserve_exact(1)
        .map_err(|_| negotiation::Error::Allocation)?;
    offer.capabilities.push(Capability {
        name: receiver_metrics::CAPABILITY.into(),
        version: receiver_metrics::VERSION,
        required: false,
    });
    offer.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    offer.validate()?;
    Ok(offer)
}

/// Offer reports of the real adaptive source's operating point. These optional
/// diagnostics yield at the existing capability ceiling; adding them never
/// removes another capability or widens the negotiation/resource limits.
pub fn with_quality_decisions(mut offer: Offer) -> Result<Offer, negotiation::Error> {
    use fr_wire::quality_decision::{CAPABILITY, VERSION};
    offer.validate()?;
    if let Some(existing) = offer.capabilities.iter().find(|cap| cap.name == CAPABILITY) {
        return if existing.version == VERSION && !existing.required {
            Ok(offer)
        } else {
            Err(negotiation::Error::RequiredCapability)
        };
    }
    if offer.capabilities.len() < negotiation::MAX_CAPABILITIES {
        offer
            .capabilities
            .try_reserve_exact(1)
            .map_err(|_| negotiation::Error::Allocation)?;
        offer.capabilities.push(Capability {
            name: CAPABILITY.into(),
            version: VERSION,
            required: false,
        });
        offer.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    }
    offer.validate()?;
    Ok(offer)
}

#[cfg(test)]
#[path = "offers/tests.rs"]
mod tests;
