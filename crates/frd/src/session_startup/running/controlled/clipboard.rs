//! Optional clipboard joined to the already initialized native input agent.
use super::ControlledHost;
use crate::clipboard_quic::{Bridge, Error, WorkerSeed};
use crate::session_startup::clipboard::Consent;
use asupersync::cx::Cx;
use fr_core::clipboard::ClipboardSwitch;
use fr_transport::quic::{
    ChannelRequest, ChannelScope, MediaChannel, QuicRecords, clipboard::ClipboardChannel,
};
use fr_wire::{clipboard::session::synchronize::Received, negotiation::ControlBinding};

pub(super) fn join(
    q: &mut QuicRecords,
    cx: &Cx,
    parent: ControlBinding,
    input: &crate::input_quic::QuicInput,
    channel: MediaChannel,
    granted: bool,
) -> Result<(Bridge, WorkerSeed), Error> {
    if channel.completed_parent(q).map_err(Error::Transport)? != parent {
        return Err(Error::WrongConnection);
    }
    let monitor = input.clipboard_monitor(q).map_err(|_| Error::Closed)?;
    let mut lane =
        ClipboardChannel::new(q, channel, monitor.binding()).map_err(Error::Transport)?;
    if !granted {
        lane.retire(q, cx).map_err(Error::Transport)?;
        return Err(Error::ConsentRequired);
    }
    Bridge::host_monitor(cx.clone(), q, lane, monitor, granted)
}
impl ControlledHost {
    /// Begin the separately selected clipboard exchange on this already running
    /// controller. `request` describes the approved current display/view and a
    /// fresh binding ID/unpredictable host ticket. It cannot grant input. Normal
    /// `drive` turns negotiate the pair while input/media/renewal keep running.
    ///
    /// `granted` is independent local clipboard consent, not input permission.
    /// Denial still completes the bounded metadata exchange, then retires only
    /// clipboard without opening the OS. Take the one-use worker seed after a
    /// successful exchange. Missing capability selection allocates nothing.
    pub fn offer_clipboard(&mut self, request: ChannelRequest, granted: bool) -> Result<(), Error> {
        if self.clipboard.is_some() || !self.clipboard_setup.available() {
            return Err(Error::AlreadyAttached);
        }
        self.session.check().map_err(|_| Error::Closed)?;
        let monitor = self
            .input
            .clipboard_monitor(&self.session.opened.transport)
            .map_err(|_| Error::Closed)?;
        let cx = &self.session.opened.cx;
        let observation = &self.session.opened.control;
        self.clipboard_setup.offer(
            cx,
            &mut self.session.opened.transport,
            ChannelScope {
                control: self.session.opened.routes,
                parent: self.session.opened.binding,
                selection: &self.session.opened.selected,
            },
            request,
            Consent {
                scope: monitor.binding(),
                granted,
            },
            || {
                observation.check().is_ok()
                    && crate::session_startup::now(cx).is_ok_and(|t| {
                        monitor
                            .deadline(fr_core::time::HostInstant::from_micros(t))
                            .is_ok()
                    })
            },
        )
    }
    pub fn clipboard_negotiating(&self) -> bool {
        self.clipboard_setup.negotiating()
    }
    /// Return once, without opening the OS. Move this seed to the interactive
    /// worker or call its `spawn` with a separately permitted native factory.
    pub fn take_clipboard_worker(&mut self) -> Option<WorkerSeed> {
        self.clipboard_setup.take_worker()
    }
    /// Consume a completed, separately negotiated clipboard pair on THIS session.
    /// `granted` requires independent local clipboard consent for the selected OS
    /// session, not just input permission. Move the seed to the native worker;
    /// the regular host drive services its network handoff during traffic/silence.
    pub fn attach_clipboard(
        &mut self,
        channel: MediaChannel,
        granted: bool,
    ) -> Result<WorkerSeed, Error> {
        if self.clipboard.is_some() || !self.clipboard_setup.available() {
            return Err(Error::AlreadyAttached);
        }
        self.session.check().map_err(|_| Error::Closed)?;
        let (bridge, seed) = join(
            &mut self.session.opened.transport,
            &self.session.opened.cx,
            self.session.opened.binding,
            &self.input,
            channel,
            granted,
        )?;
        self.clipboard = Some(bridge);
        Ok(seed)
    }
    pub fn clipboard_switches(&self) -> Option<(ClipboardSwitch, ClipboardSwitch)> {
        self.clipboard.as_ref().map(Bridge::switches)
    }
    pub fn clipboard_retired(&self) -> bool {
        self.clipboard.as_ref().is_some_and(Bridge::is_retired)
            || self.clipboard_setup.reason().is_some()
    }
    pub fn clipboard_reason(&self) -> Option<Error> {
        self.clipboard
            .as_ref()
            .and_then(Bridge::reason)
            .or(self.clipboard_setup.reason())
    }
    pub fn take_clipboard_received(&mut self) -> Result<Option<Received>, Error> {
        self.clipboard
            .as_mut()
            .map_or(Ok(None), Bridge::take_received)
    }
    /// Stop only completed clipboard between I/O turns. An incomplete exchange
    /// has not yet established an optional pair: cancelling it conservatively
    /// closes the session instead of making its reserved stream IDs reusable.
    pub fn retire_clipboard(&mut self) -> Result<(), Error> {
        if self.clipboard_setup.negotiating() {
            self.close();
            return Err(Error::Cancelled);
        }
        if let Some(bridge) = &mut self.clipboard {
            bridge.retire(&mut self.session.opened.transport)?;
        }
        self.clipboard_setup.stop();
        Ok(())
    }
}
