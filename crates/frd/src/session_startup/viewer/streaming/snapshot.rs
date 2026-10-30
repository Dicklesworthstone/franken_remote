//! One explicit initial artifact, before entering the continuing viewer loop.
//! Native pixel export shares the existing network turn; host revocation,
//! renewal and recovery keep progressing. It never creates a controller.
use super::{Error, Peer, PresentationStage, StreamingViewer};
use fr_media::{
    access_unit::FrameId,
    delivery::ReceiveState,
    worker::presentation_snapshot::{Snapshot, Stamp},
};
use std::{future::Future, pin::pin};

struct Attempt<'a> {
    viewer: &'a mut StreamingViewer,
    complete: bool,
}
impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.viewer.close();
        }
    }
}
impl StreamingViewer {
    /// Creation owns cancellation even if the returned future is never polled.
    /// At most one export on an observation-only startup; there is no hidden
    /// request loop, retry, replacement observation or authority acquisition.
    pub(crate) fn snapshot_initial(
        &mut self,
        frame: FrameId,
    ) -> impl Future<Output = Result<Option<Snapshot>, Error>> + '_ {
        let allowed = !self.served
            && !self.initial_snapshot_requested
            && matches!(self.peer, Peer::Observe { .. });
        self.initial_snapshot_requested = true;
        let mut attempt = Attempt {
            viewer: self,
            complete: false,
        };
        async move {
            if !allowed {
                return Err(Error::RequestAlreadyStarted);
            }
            let result = attempt.viewer.snapshot_inner(frame).await;
            attempt.complete = result.is_ok();
            result
        }
    }
    async fn snapshot_inner(&mut self, frame: FrameId) -> Result<Option<Snapshot>, Error> {
        let initial = self.initial.as_ref().ok_or(Error::Closed)?;
        if initial.frame != frame {
            return Err(Error::Closed);
        }
        if initial.stage != PresentationStage::SubmittedToCompositor {
            return Ok(None);
        }
        let original = self.snapshot_binding()?;
        let expected = Stamp {
            frame,
            configuration: original.configuration,
            recovery: original.recovery,
        };
        let cx = self.control.cx.clone();
        let snapshot = {
            let mut native = pin!(self.presenter.read_presentation(&cx, expected));
            let mut result = |_| {};
            let mut other = |_, _: &[u8]| Err(());
            loop {
                let turn = super::network(
                    &mut self.peer,
                    self.clipboard.as_mut(),
                    self.files.as_mut(),
                    &mut self.receiver,
                    &mut self.repair,
                    self.recovery.as_deref_mut(),
                    &mut self.statistics,
                    self.feedback.as_mut(),
                    self.presentation.as_mut(),
                    self.cursor.as_mut(),
                    self.audio.as_mut(),
                    &cx,
                    &mut result,
                    &mut other,
                );
                if let Some(snapshot) =
                    super::native_turn::drive(native.as_mut(), turn, &self.control).await?
                {
                    break snapshot;
                }
            }
        };
        // The worker's completion is not authorization. A cancelled or retired
        // receiver, changed media binding, or parent failure discards all bytes.
        if self.snapshot_binding()? != original {
            return Err(Error::Closed);
        }
        Ok(snapshot)
    }
    fn snapshot_binding(&mut self) -> Result<fr_wire::decoder::Binding, Error> {
        if self.control.is_stopped() || self.receiver.state() != ReceiveState::Streaming {
            return Err(Error::Closed);
        }
        let (session, media) = self.peer.parts()?;
        self.presenter
            .check_stream(&session.transport, media, &self.receiver)
            .map_err(Error::Media)?;
        Ok(media.binding())
    }
}
