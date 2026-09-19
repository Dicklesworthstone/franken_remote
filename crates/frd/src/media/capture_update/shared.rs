//! Bounded distribution of actual worker results, not caller-made source proofs.
use super::{CaptureUpdate, Content, Error, Subscription};
use crate::media_egress::Egress;
use fr_core::ids::CodecConfigurationGeneration;
use fr_media::{
    access_unit::FrameId,
    delivery::{DeliveryMode, SharedFrame, SharedFramePool, SharedFrameReservation},
    worker::UnchangedCapture,
};
use std::sync::Arc;

/// Maximum recipients serviced by one synchronous fanout turn. This is not a
/// broker admission limit: the OS share-session must separately bound its registry.
pub const MAX_FANOUT_RECIPIENTS: usize = 8;
#[derive(Clone)]
enum SharedContent {
    Encoded(SharedFrame),
    Unchanged(UnchangedCapture),
}
/// Only an actual `CaptureUpdate` can create this source-bound result. Cloning
/// retains one physical compressed allocation or copies fixed unchanged metadata;
/// no worker, authority, connection, or input lease is cloned or granted.
#[derive(Clone)]
pub struct SharedCaptureUpdate {
    source: Arc<()>,
    configuration: CodecConfigurationGeneration,
    content: SharedContent,
}
impl std::fmt::Debug for SharedCaptureUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedCaptureUpdate")
            .field("frame", &self.frame())
            .field("unchanged", &self.is_unchanged())
            .finish_non_exhaustive()
    }
}
impl CaptureUpdate {
    /// Transfer this already bounded worker result into the share-session pool.
    /// The caller must retain the SAME pool across viewers and recoveries, and
    /// admit capture capacity before native production. This neither chooses a
    /// display nor permits a second viewer to observe a previously selected one.
    pub fn share(self, pool: &SharedFramePool) -> Result<SharedCaptureUpdate, Error> {
        let content = match self.content {
            Content::Encoded(unit) => {
                SharedContent::Encoded(pool.share(unit).map_err(Error::Receiver)?)
            }
            Content::Unchanged(proof) => SharedContent::Unchanged(proof),
        };
        Ok(SharedCaptureUpdate {
            source: self.source,
            configuration: self.configuration,
            content,
        })
    }
}
impl CaptureUpdate {
    /// Finish a pre-admitted native capture on its original physical reservation.
    /// A static observation releases unused picture/byte credit immediately.
    pub fn share_reserved(
        self,
        reservation: SharedFrameReservation,
    ) -> Result<SharedCaptureUpdate, Error> {
        let content = match self.content {
            Content::Encoded(unit) => {
                SharedContent::Encoded(reservation.share(unit).map_err(Error::Receiver)?)
            }
            Content::Unchanged(proof) => {
                drop(reservation);
                SharedContent::Unchanged(proof)
            }
        };
        Ok(SharedCaptureUpdate {
            source: self.source,
            configuration: self.configuration,
            content,
        })
    }
}
impl SharedCaptureUpdate {
    pub fn frame(&self) -> FrameId {
        match &self.content {
            SharedContent::Encoded(frame) => frame.frame(),
            SharedContent::Unchanged(proof) => proof.reference,
        }
    }
    pub fn observed_micros(&self) -> u64 {
        match &self.content {
            SharedContent::Encoded(frame) => frame.capture_micros(),
            SharedContent::Unchanged(proof) => proof.observed_micros,
        }
    }
    pub fn is_unchanged(&self) -> bool {
        matches!(self.content, SharedContent::Unchanged(_))
    }
    pub fn encoded(&self) -> Option<&SharedFrame> {
        match &self.content {
            SharedContent::Encoded(frame) => Some(frame),
            SharedContent::Unchanged(_) => None,
        }
    }
    /// Service every already-admitted egress independently, even when an earlier
    /// viewer refuses. No packet queue, per-turn allocation, or async/native work
    /// is created here. Each egress still owns its transport pacing and final
    /// authority check. Index i in the report describes only recipient i.
    pub fn distribute(&self, recipients: &mut [&mut Egress]) -> Result<FanoutReport, Error> {
        if recipients.is_empty() || recipients.len() > MAX_FANOUT_RECIPIENTS {
            return Err(Error::InvalidFrame);
        }
        let mut report = FanoutReport {
            results: [Ok(()); MAX_FANOUT_RECIPIENTS],
            len: recipients.len(),
        };
        for (result, recipient) in report.results.iter_mut().zip(recipients) {
            *result = recipient.enqueue_shared_capture(self);
        }
        Ok(report)
    }
}
/// Fixed-size per-recipient admission outcomes, not transport or decode receipts.
#[derive(Debug)]
pub struct FanoutReport {
    results: [Result<(), Error>; MAX_FANOUT_RECIPIENTS],
    len: usize,
}
impl FanoutReport {
    pub fn results(&self) -> &[Result<(), Error>] {
        &self.results[..self.len]
    }
}
impl Subscription {
    /// Accept actual source-bound output with this viewer's own authority, limits,
    /// reference chain and deadlines. No new source is inferred from equal frame
    /// IDs. As in `enqueue_capture`, the parent must have admitted this source/view
    /// before the first IDR. A first unchanged result cannot establish provenance.
    ///
    /// After accepting the source, a dropped reference or expired authority fences
    /// THIS cache and input readiness instead of pinning shared history or silently
    /// skipping encoded frames. Wrong-source/configuration preflight leaves it
    /// untouched. Parent connection retirement/OS input cleanup remains required.
    pub fn enqueue_shared_capture(&mut self, update: &SharedCaptureUpdate) -> Result<(), Error> {
        if update.configuration != self.epoch.configuration
            || self
                .capture_source
                .as_ref()
                .is_some_and(|source| !Arc::ptr_eq(source, &update.source))
            || (self.capture_source.is_none() && update.is_unchanged())
        {
            return Err(Error::InvalidFrame);
        }
        let result = (|| {
            let now = self.control.check()?.as_micros();
            match &update.content {
                SharedContent::Encoded(frame) => {
                    self.cache.push_shared(
                        frame,
                        if self.first {
                            DeliveryMode::Recovery
                        } else {
                            DeliveryMode::Datagrams
                        },
                        now,
                    )?;
                    self.first = false;
                    self.capture_source = Some(update.source.clone());
                }
                SharedContent::Unchanged(proof) => {
                    self.cache.observe_unchanged(
                        proof.reference.as_raw(),
                        proof.observed_micros,
                        now,
                    )?;
                }
            }
            Ok(())
        })();
        if result.is_err() {
            // Do not cancel the source's Cx or another viewer. Final OS input
            // submission checks this very authority mutex, so old tickets end
            // before this admission failure is returned to the caller.
            if let Ok(mut authority) = self.control.authority.lock() {
                authority.mark_view_stale();
            }
            self.cache.close();
        }
        result
    }
}

impl Subscription {
    pub(crate) fn shared_identity(&self) -> Result<Arc<()>, Error> {
        if self.first {
            return Err(Error::InvalidFrame);
        }
        self.capture_source.clone().ok_or(Error::InvalidFrame)
    }
    pub(crate) fn fence_shared_view(&mut self) {
        if let Ok(mut authority) = self.control.authority.lock() {
            authority.mark_view_stale();
        }
        self.cache.close();
    }
}
impl SharedCaptureUpdate {
    pub(crate) fn belongs_to_shared_source(&self, source: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.source, source)
    }
}
