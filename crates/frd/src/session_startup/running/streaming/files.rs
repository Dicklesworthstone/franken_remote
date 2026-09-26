//! `frd run --files DIR`: the controller's drop lane on the ORIGINAL
//! controlled share. Nothing opens before the grant: this owner only acts on
//! a `ControlledHost`, i.e. after the input lease exists.
//!
//! The one-use offer waits for the first control-lease renewal. A viewer
//! answers a control challenge only from its promoted controller, and the
//! viewer installs its drop expectation before that controller's first drive
//! (`viewer::streaming::files`), so the offer can never reach a viewer that is
//! not expecting it (an unexpected attachment record would fail its session).
//! The disk worker, byte limits and lease checks are `fr_files`' own; revoke
//! or expiry fences the lane and a staged file is removed, never published.
use super::ControlledHost;
use crate::native_files::{Directory, SETUP_TIMEOUT};
use fr_files::quic::Configuration;
use fr_transport::quic::ChannelRequest;
use fr_wire::{attachment::Ticket, decoder::Binding, negotiation::ControlBinding};

/// The streaming loop's between-turn hook: only a controlled session (after
/// the grant) can carry the lane; observation never offers files.
pub(super) fn between_turns(
    lane: Option<&mut Lane>,
    host: &mut super::Host,
    view: Binding,
    nonce: &mut impl FnMut() -> Result<u128, ()>,
) {
    if let (Some(lane), super::Host::Control(host)) = (lane, host) {
        lane.host(host, view, nonce);
    }
}

pub(in crate::session_startup) struct Lane {
    /// Taken by the one offer attempt; never re-offered after any outcome.
    configuration: Option<Configuration>,
}
impl Lane {
    pub(in crate::session_startup) fn new(directory: &Directory) -> Self {
        Self {
            configuration: Some(directory.configuration()),
        }
    }
    /// Between complete network turns, never inside a transport callback.
    /// A failed offer leaves desktop control untouched and is not retried.
    pub(in crate::session_startup) fn host(
        &mut self,
        host: &mut ControlledHost,
        view: Binding,
        nonce: &mut impl FnMut() -> Result<u128, ()>,
    ) {
        if self.configuration.is_none() || host.control_renewed_until().is_none() {
            return;
        }
        let Some(configuration) = self.configuration.take() else {
            return;
        };
        let id = match host.io() {
            Ok((q, _)) => q.next_channel_binding().ok().filter(|&id| id != 0),
            Err(_) => None,
        };
        let ticket = nonce().ok().filter(|&ticket| ticket != 0);
        let (Some(id), Some(ticket)) = (id, ticket) else {
            return;
        };
        let request = ChannelRequest {
            binding: Binding {
                parent: ControlBinding { id, ..view.parent },
                ..view
            },
            ticket: Ticket(ticket),
            timeout: SETUP_TIMEOUT,
        };
        // Refusals are typed on the ControlledHost (`file_receive_reason`);
        // the local permission is fresh, so only transport/clock state or a
        // missing selection can refuse here.
        let _ = host.offer_file_drop(request, configuration);
    }
}
