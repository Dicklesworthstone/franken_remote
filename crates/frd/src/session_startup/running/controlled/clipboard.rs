//! Optional clipboard joined to the already initialized native input agent.
use super::ControlledHost;
use crate::clipboard_quic::{Bridge, Error, WorkerSeed};
use fr_core::clipboard::ClipboardSwitch;
use fr_transport::quic::{MediaChannel, clipboard::ClipboardChannel};
use fr_wire::clipboard::session::synchronize::Received;

impl ControlledHost {
    /// Consume a completed, separately negotiated clipboard pair on THIS session.
    /// `granted` requires independent local clipboard consent for the selected OS
    /// session, not just input permission. Move the seed to the native worker;
    /// the regular host drive services its network handoff during traffic/silence.
    pub fn attach_clipboard(
        &mut self,
        channel: MediaChannel,
        granted: bool,
    ) -> Result<WorkerSeed, Error> {
        if self.clipboard.is_some() {
            return Err(Error::AlreadyAttached);
        }
        self.session.check().map_err(|_| Error::Closed)?;
        let q = &self.session.opened.transport;
        if channel.completed_parent(q).map_err(Error::Transport)? != self.session.opened.binding {
            return Err(Error::WrongConnection);
        }
        let monitor = self.input.clipboard_monitor(q).map_err(|_| Error::Closed)?;
        let mut lane =
            ClipboardChannel::new(q, channel, monitor.binding()).map_err(Error::Transport)?;
        if !granted {
            lane.retire(&mut self.session.opened.transport, &self.session.opened.cx)
                .map_err(Error::Transport)?;
            return Err(Error::ConsentRequired);
        }
        let q = &self.session.opened.transport;
        let (bridge, seed) =
            Bridge::host_monitor(self.session.opened.cx.clone(), q, lane, monitor, granted)?;
        self.clipboard = Some(bridge);
        Ok(seed)
    }
    pub fn clipboard_switches(&self) -> Option<(ClipboardSwitch, ClipboardSwitch)> {
        self.clipboard.as_ref().map(Bridge::switches)
    }
    pub fn clipboard_retired(&self) -> bool {
        self.clipboard.as_ref().is_some_and(Bridge::is_retired)
    }
    pub fn clipboard_reason(&self) -> Option<Error> {
        self.clipboard.as_ref().and_then(Bridge::reason)
    }
    pub fn take_clipboard_received(&mut self) -> Result<Option<Received>, Error> {
        self.clipboard
            .as_mut()
            .map_or(Ok(None), Bridge::take_received)
    }
    /// Stop only clipboard between I/O turns. Keep the owner/receipt tombstone;
    /// input and viewing continue and a second lane cannot reset replay history.
    pub fn retire_clipboard(&mut self) -> Result<(), Error> {
        if let Some(bridge) = &mut self.clipboard {
            bridge.retire(&mut self.session.opened.transport)?;
        }
        Ok(())
    }
}
