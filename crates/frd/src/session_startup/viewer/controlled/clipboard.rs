//! Optional clipboard uses this running viewer's actual presentation owner.
use super::{ControlledViewer, Error as ViewerError, permitted};
use crate::clipboard_quic::{Bridge, Error, WorkerSeed};
use fr_core::clipboard::{Binding, ClipboardSwitch};
use fr_transport::quic::{MediaChannel, clipboard::ClipboardChannel};
use fr_wire::clipboard::session::synchronize::Received;

impl ControlledViewer {
    /// Join the completed clipboard pair to the SAME decoder-backed grant.
    /// Independent native clipboard consent is required before the returned seed
    /// can open the OS. Normal `drive` calls service the handoff automatically.
    pub fn attach_clipboard(
        &mut self,
        channel: MediaChannel,
        granted: bool,
    ) -> Result<WorkerSeed, Error> {
        if self.clipboard.is_some() {
            return Err(Error::AlreadyAttached);
        }
        self.check().map_err(|_| Error::Closed)?;
        let q = &self.session.transport;
        if channel.completed_parent(q).map_err(Error::Transport)? != self.session.opened.binding {
            return Err(Error::WrongConnection);
        }
        let scope = Binding {
            session: self.input.binding().session,
            lease: self.input.binding().lease,
        };
        let mut lane = ClipboardChannel::new(q, channel, scope).map_err(Error::Transport)?;
        if !granted {
            lane.retire(&mut self.session.transport, &self.session.cx)
                .map_err(Error::Transport)?;
            return Err(Error::ConsentRequired);
        }
        let q = &self.session.transport;
        let (bridge, seed) =
            Bridge::presented(self.session.cx.clone(), q, lane, &mut self.input, granted)?;
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
    pub fn retire_clipboard(&mut self) -> Result<(), Error> {
        if let Some(bridge) = &mut self.clipboard {
            bridge.retire(&mut self.session.transport)?;
        }
        Ok(())
    }
    pub(super) fn service_clipboard(&mut self) -> Result<(), ViewerError> {
        if let Some(bridge) = &mut self.clipboard {
            bridge
                .service(&mut self.session.transport, || {
                    permitted(&mut self.input, &self.control, &self.session.cx)
                })
                .map_err(ViewerError::Clipboard)?;
        }
        Ok(())
    }
}
