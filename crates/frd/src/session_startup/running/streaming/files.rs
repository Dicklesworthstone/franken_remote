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
use crate::{
    native_files::{Directory, HostDownload, SETUP_TIMEOUT, SendControl, SendRequest},
    session_startup::viewer::streaming::files::{Endpoint, Sending},
};
use fr_files::{
    quic::Configuration,
    sender::{self, Policy, Progress, Stage, batch::Report},
    session::Permission,
};
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
    interactive: Option<Box<Sending>>,
}
impl Lane {
    pub(in crate::session_startup) fn has_upload(&self) -> bool {
        self.configuration.is_some()
    }
    pub(in crate::session_startup) fn has_download(&self) -> bool {
        self.selection.is_some() || self.interactive.is_some()
    }
    pub(in crate::session_startup) fn add_upload(&mut self, directory: &Directory) {
        self.configuration = Some(directory.configuration());
    }
    pub(in crate::session_startup) fn add_download(&mut self, selection: SendRequest) {
        self.selection = Some(selection);
    }
    pub(in crate::session_startup) fn add_host_download(&mut self, selected: HostDownload) {
        match selected {
            HostDownload::Initial(selection) => self.add_download(selection),
            HostDownload::Interactive(intake) => {
                let mut sending = Sending::deferred(Policy::default(), SendControl::new());
                sending.set_intake(intake);
                self.interactive = Some(Box::new(sending));
            }
        }
    }
    pub(in crate::session_startup) fn close(&mut self) {
        self.selection = None;
        if let Some(sending) = &mut self.interactive {
            sending.stop();
        }
    }
    pub(in crate::session_startup) fn observe(&self, host: &mut ControlledHost) {
        if let Some(sending) = &self.interactive {
            sending.poll_cleanup(&mut HostEndpoint {
                host,
                request: None,
            });
        }
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
        if let Some(sending) = &mut self.interactive {
            let request = sending
                .needs_attachment()
                .then(|| request(host, view, nonce))
                .flatten();
            sending.viewer(&mut HostEndpoint { host, request });
        }
    }
}

/// The shared selection owner uses the original host controller's admission,
/// sender and cleanup methods. Only the initial offer consumes a fresh ticket.
struct HostEndpoint<'a> {
    host: &'a mut ControlledHost,
    request: Option<ChannelRequest>,
}
impl Endpoint for HostEndpoint<'_> {
    fn expect(&mut self, policy: Policy) -> Result<(), sender::Error> {
        self.host
            .offer_file_download_idle(
                self.request.take().ok_or(sender::Error::Closed)?,
                Permission::new(true),
                policy,
            )
            .map_err(super::super::controlled::files::send_error)
    }
    fn closed(&self) -> bool {
        self.host.control().is_stopped() || self.host.session.opened.transport.is_closed()
    }
    fn negotiating(&self) -> bool {
        self.host.file_send_negotiating()
    }
    fn stage(&self) -> Option<Stage> {
        self.host.file_send_stage()
    }
    fn failure(&self) -> Option<sender::Error> {
        self.host.file_send_failure()
    }
    fn start(&mut self, request: SendRequest) -> Result<(), sender::Error> {
        self.host
            .send_download_files(request.selection, request.lifetime)
    }
    fn start_before(&mut self, request: SendRequest, until_us: u64) -> Result<(), sender::Error> {
        self.host
            .send_download_files_before(request.selection, until_us)
    }
    fn collect_report(&mut self) -> Option<Report> {
        self.host.take_file_batch_report()
    }
    fn can_start(&mut self) -> bool {
        self.host.can_select_download_files()
    }
    fn cancel(&mut self) -> Result<(), sender::Error> {
        self.host
            .retire_file_download()
            .map_err(super::super::controlled::files::send_error)
    }
    fn cleanup(&mut self) -> Option<Result<(), sender::Error>> {
        self.host.try_finish_download_cleanup()
    }
    fn report(&self) -> Option<Report> {
        self.host.file_batch_report()
    }
    fn progress(&self) -> Option<Progress> {
        self.host.file_send_progress()
    }
    fn set_paused(&mut self, paused: bool) -> Result<(), sender::Error> {
        self.host.set_download_paused(paused)
    }
    fn paused(&self) -> Option<bool> {
        self.host.download_is_paused()
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
