//! Optional clipboard uses this running viewer's actual presentation owner.
use super::{ControlledViewer, Error as ViewerError, permitted};
use crate::clipboard_quic::{Bridge, Error, WorkerSeed};
use crate::session_startup::clipboard::Consent;
use fr_core::clipboard::{Binding, ClipboardSwitch};
use fr_transport::quic::{ChannelScope, MediaChannel, clipboard::ClipboardChannel};
use fr_wire::clipboard::session::synchronize::Received;
use std::time::Duration;

impl ControlledViewer {
    /// Expect one host-initiated clipboard exchange on this running controller.
    /// This snapshots the real selected media tuple; a different display/view
    /// cannot be substituted by the offer. The timeout starts NOW, not at the
    /// first drive or when an offer finally arrives. Call only after positive
    /// optional capability selection. No native clipboard is opened here.
    ///
    /// Normal drives complete the exchange without blocking input or media.
    /// Independent local consent is still required: false retires the completed
    /// pair with `ConsentRequired` and never produces a native-worker seed.
    pub fn expect_clipboard(&mut self, timeout: Duration, granted: bool) -> Result<(), Error> {
        if self.clipboard.is_some()
            || !self.clipboard_setup.available()
            || self.file_send_negotiating()
        {
            return Err(Error::AlreadyAttached);
        }
        self.check().map_err(|_| Error::Closed)?;
        if !permitted(&mut self.input, &self.control, &self.session.cx) {
            return Err(Error::Closed);
        }
        self.clipboard_setup.expect(
            &self.session.cx,
            &self.session.transport,
            ChannelScope {
                control: self.session.routes,
                parent: self.session.opened.binding,
                selection: &self.session.opened.selection,
            },
            self.media.binding(),
            timeout,
            Consent {
                scope: Binding {
                    session: self.input.binding().session,
                    lease: self.input.binding().lease,
                },
                granted,
            },
        )
    }
    pub fn clipboard_negotiating(&self) -> bool {
        self.clipboard_setup.negotiating()
    }
    /// Single-use delivery. Merely obtaining this seed performs no OS operation;
    /// closing the session or dropping it still fences native opening/publication.
    pub fn take_clipboard_worker(&mut self) -> Option<WorkerSeed> {
        self.clipboard_setup.take_worker()
    }

    /// Join the completed clipboard pair to the SAME decoder-backed grant.
    /// Independent native clipboard consent is required before the returned seed
    /// can open the OS. Normal `drive` calls service the handoff automatically.
    pub fn attach_clipboard(
        &mut self,
        channel: MediaChannel,
        granted: bool,
    ) -> Result<WorkerSeed, Error> {
        if self.clipboard.is_some()
            || !self.clipboard_setup.available()
            || self.file_send_negotiating()
        {
            return Err(Error::AlreadyAttached);
        }
        self.check().map_err(|_| Error::Closed)?;
        self.join_clipboard(channel, granted)
    }
    fn join_clipboard(
        &mut self,
        channel: MediaChannel,
        granted: bool,
    ) -> Result<WorkerSeed, Error> {
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
    pub fn retire_clipboard(&mut self) -> Result<(), Error> {
        if self.clipboard_setup.negotiating() {
            self.close();
            return Err(Error::Cancelled);
        }
        if let Some(bridge) = &mut self.clipboard {
            bridge.retire(&mut self.session.transport)?;
        }
        self.clipboard_setup.stop();
        Ok(())
    }
    pub(super) fn service_clipboard(&mut self) -> Result<(), ViewerError> {
        if let Some((channel, granted)) = self
            .clipboard_setup
            .service(&mut self.session.transport, || {
                permitted(&mut self.input, &self.control, &self.session.cx)
            })
            .map_err(ViewerError::Clipboard)?
        {
            let result = self.join_clipboard(channel, granted);
            self.clipboard_setup
                .joined(result)
                .map_err(ViewerError::Clipboard)?;
        }
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
