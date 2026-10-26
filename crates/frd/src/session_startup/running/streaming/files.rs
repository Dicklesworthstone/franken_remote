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
use crate::native_files::{Directory, SETUP_TIMEOUT, SendRequest};
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

#[derive(Default)]
pub(in crate::session_startup) struct Lane {
    /// Taken by each direction's one offer attempt; never retried after outcome.
    configuration: Option<Configuration>,
    selection: Option<SendRequest>,
}
impl Lane {
    pub(in crate::session_startup) fn has_upload(&self) -> bool {
        self.configuration.is_some()
    }
    pub(in crate::session_startup) fn has_download(&self) -> bool {
        self.selection.is_some()
    }
    pub(in crate::session_startup) fn add_upload(&mut self, directory: &Directory) {
        self.configuration = Some(directory.configuration());
    }
    pub(in crate::session_startup) fn add_download(&mut self, selection: SendRequest) {
        self.selection = Some(selection);
    }
    /// Between complete network turns. Both independent offers wait for the
    /// original lease renewal; each consumes its own fresh ticket and binding.
    pub(in crate::session_startup) fn host(
        &mut self,
        host: &mut ControlledHost,
        view: Binding,
        nonce: &mut impl FnMut() -> Result<u128, ()>,
    ) {
        if host.control_renewed_until().is_none() {
            return;
        }
        if let Some(configuration) = self.configuration.take()
            && let Some(request) = request(host, view, nonce)
        {
            let _ = host.offer_file_drop(request, configuration);
        }
        if let Some(selection) = self.selection.take()
            && let Some(request) = request(host, view, nonce)
        {
            let _ = host.offer_file_download(
                request,
                selection.selection,
                fr_files::session::Permission::new(true),
                selection.policy,
                selection.lifetime,
            );
        }
    }
}
fn request(
    host: &mut ControlledHost,
    view: Binding,
    nonce: &mut impl FnMut() -> Result<u128, ()>,
) -> Option<ChannelRequest> {
    let id = host
        .io()
        .ok()?
        .0
        .next_channel_binding()
        .ok()
        .filter(|&id| id != 0)?;
    let ticket = nonce().ok().filter(|&ticket| ticket != 0)?;
    Some(ChannelRequest {
        binding: Binding {
            parent: ControlBinding { id, ..view.parent },
            ..view
        },
        ticket: Ticket(ticket),
        timeout: SETUP_TIMEOUT,
    })
}
