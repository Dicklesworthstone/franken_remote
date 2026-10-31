//! On-demand pixel export on the ORIGINAL continuing observer/controller.
//! One mailbox reservation spans native work and local response delivery.
//! No per-frame copy, new connection, input authority, or disk pathname.
use super::{Error, PresentationStage, StreamingViewer, acquisition, native_turn, network};
use asupersync::cx::Cx;
use fr_client::{
    input::ResultEvent,
    robot::snapshot::{Channel, Error as Refusal, Handle},
};
use fr_media::{
    access_unit::FrameId,
    delivery::{ReceivePipeline, ReceiveState},
    worker::presentation_snapshot::Stamp,
};
use fr_transport::quic::{Disposition, Route};
use std::{pin::pin, time::Instant};

/// Retire at the exact receiver transition, including while another native
/// operation is pending. A delivered image must not outlive its recovery scope.
pub(super) fn retire_stale(receiver: &ReceivePipeline, channel: Option<&Channel>) {
    if receiver.state() != ReceiveState::Streaming
        && let Some(channel) = channel
    {
        channel.publish(None);
    }
}

impl StreamingViewer {
    /// Explicit local opt-in before serving. The returned handle cannot dial,
    /// capture, grant input or replace this owner; closing the viewer retires it.
    pub(crate) fn snapshot_handle(&mut self) -> Result<Handle, Error> {
        if self.served || self.control.is_stopped() {
            return Err(Error::RequestAlreadyStarted);
        }
        self.snapshot_binding()?;
        let control = self.control.clone();
        let (channel, handle) = self
            .snapshots
            .get_or_insert_with(|| Channel::with_liveness(move || !control.is_stopped()));
        channel.publish(self.snapshot_stamp);
        Ok(handle.clone())
    }
    /// Called only after an actual completion has passed the original receiver
    /// admission and generation gates. A reference-only completion is not a new
    /// presented image and cannot replace the worker's retained viewport stamp.
    pub(super) fn note_snapshot(
        &mut self,
        frame: FrameId,
        stage: PresentationStage,
    ) -> Result<(), Error> {
        if stage != PresentationStage::SubmittedToCompositor {
            return Ok(());
        }
        let (_, media) = self.peer.parts()?;
        let binding = media.binding();
        self.snapshot_stamp = Some(Stamp {
            frame,
            configuration: binding.configuration,
            recovery: binding.recovery,
        });
        if let Some((channel, _)) = &mut self.snapshots {
            channel.publish(self.snapshot_stamp);
        }
        Ok(())
    }
    /// A failed/replaced chain cannot authorize export of old retained pixels.
    /// A delivered historical image may finish only within its original scope.
    pub(super) fn clear_snapshot(&mut self) {
        self.snapshot_stamp = None;
        if let Some((channel, _)) = &mut self.snapshots {
            channel.publish(None);
        }
    }
    pub(super) fn close_snapshots(&mut self) {
        self.snapshot_stamp = None;
        if let Some((channel, _)) = &mut self.snapshots {
            channel.close();
        }
    }
    /// Between native jobs, take at most one explicit request. A reader's drop
    /// cancels its result, not this controller: an already-started worker IPC
    /// operation drains under its original native ceiling while network and
    /// input callbacks keep running. Late completion cannot renew the request.
    pub(super) async fn service_snapshot(
        &mut self,
        cx: &Cx,
        ui: &mut impl FnMut(acquisition::Dispatch<'_>, Option<super::Presentation>) -> Result<(), ()>,
        result: &mut impl FnMut(ResultEvent),
        other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<(), Error> {
        let Some(work) = self
            .snapshots
            .as_mut()
            .and_then(|(channel, _)| channel.take())
        else {
            return Ok(());
        };
        if Instant::now() >= work.deadline() {
            work.complete(Err(Refusal::Expired));
            return Ok(());
        }
        if self.receiver.state() != ReceiveState::Streaming {
            work.complete(Err(Refusal::Unavailable));
            return Ok(());
        }
        let expected = work.stamp();
        if self.snapshot_stamp != Some(expected) {
            work.complete(Err(Refusal::NotCurrent));
            return Ok(());
        }
        let original = self.snapshot_binding()?;
        let snapshot = {
            // No requester deadline is restarted. This is the single native
            // operation's existing supervision ceiling; mailbox completion
            // independently enforces the earlier request's absolute deadline.
            let mut native = pin!(self.presenter.read_presentation(cx, expected));
            loop {
                let turn = network(
                    &mut self.peer,
                    self.clipboard.as_mut(),
                    self.files.as_mut(),
                    &mut self.receiver,
                    self.snapshots.as_ref().map(|(channel, _)| channel),
                    &mut self.repair,
                    self.recovery.as_deref_mut(),
                    &mut self.statistics,
                    self.feedback.as_mut(),
                    self.presentation.as_mut(),
                    self.cursor.as_mut(),
                    self.audio.as_mut(),
                    cx,
                    result,
                    other,
                );
                if let Some(snapshot) =
                    native_turn::drive(native.as_mut(), turn, &self.control).await?
                {
                    break snapshot;
                }
                // A queued keyboard chord still receives receipts and checks
                // its original deadlines while this optional image is pending.
                acquisition::notify(&mut self.peer, &self.receiver, &mut self.control, None, ui)?;
            }
        };
        if self.receiver.state() != ReceiveState::Streaming {
            self.clear_snapshot();
            work.complete(Err(Refusal::NotCurrent));
            return Ok(());
        }
        // Native completion is not authorization. Parent silence, revocation,
        // replacement and receiver ownership are checked before pixels escape.
        if self.snapshot_binding()? != original || self.snapshot_stamp != Some(expected) {
            work.complete(Err(Refusal::NotCurrent));
            return Ok(());
        }
        work.complete(snapshot.ok_or(Refusal::Unavailable));
        Ok(())
    }
}
