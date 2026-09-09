//! Actual worker results with one source identity, not caller-made idle proofs.
use super::{
    CaptureSource, EncodedAccessUnit, Error, FrameId, MediaOperation, ObservationControl,
    Subscription, worker,
};
use asupersync::time::sleep;
use fr_core::ids::CodecConfigurationGeneration;
use fr_media::worker::{Kind, UnchangedCapture, capture_payload, parse_unit};
use std::{fmt, sync::Arc, time::Duration};

/// A complete worker result. Only `CaptureSource` can construct this proof. It
/// owns either one encoded buffer or fixed metadata; no raw capture escapes the
/// worker. Taking encoded bytes out deliberately loses source provenance.
pub struct CaptureUpdate {
    source: Arc<()>,
    configuration: CodecConfigurationGeneration,
    content: Content,
}
enum Content {
    Encoded(EncodedAccessUnit),
    Unchanged(UnchangedCapture),
}
impl fmt::Debug for CaptureUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaptureUpdate")
            .field("frame", &self.frame())
            .field("unchanged", &self.is_unchanged())
            .finish_non_exhaustive()
    }
}
impl CaptureUpdate {
    /// Last actually encoded picture, not the skipped capture candidate.
    pub fn frame(&self) -> FrameId {
        match &self.content {
            Content::Encoded(unit) => unit.frame(),
            Content::Unchanged(proof) => proof.reference,
        }
    }
    pub fn observed_micros(&self) -> u64 {
        match &self.content {
            Content::Encoded(unit) => unit.capture_micros(),
            Content::Unchanged(proof) => proof.observed_micros,
        }
    }
    pub fn is_unchanged(&self) -> bool {
        matches!(self.content, Content::Unchanged(_))
    }
    pub fn encoded(&self) -> Option<&EncodedAccessUnit> {
        match &self.content {
            Content::Encoded(unit) => Some(unit),
            Content::Unchanged(_) => None,
        }
    }
}
impl CaptureSource {
    /// Unconditional capture retained for callers that explicitly need a new
    /// encoded picture. This does not establish a subscription's source binding.
    pub async fn capture(
        &mut self,
        control: &ObservationControl,
        force_idr: bool,
    ) -> Result<EncodedAccessUnit, Error> {
        match self
            .capture_request(control, force_idr, false)
            .await?
            .content
        {
            Content::Encoded(unit) => Ok(unit),
            Content::Unchanged(_) => Err(Error::InvalidFrame),
        }
    }
    /// Full native comparison on each caller-scheduled observation. Static
    /// pixels produce no HEVC access unit; `force_idr` always requests real video.
    /// Schedule independently of viewer traffic; polling a cache is not evidence.
    pub async fn capture_if_changed(
        &mut self,
        control: &ObservationControl,
        force_idr: bool,
    ) -> Result<CaptureUpdate, Error> {
        self.capture_request(control, force_idr, true).await
    }
    async fn capture_request(
        &mut self,
        control: &ObservationControl,
        force_idr: bool,
        conditional: bool,
    ) -> Result<CaptureUpdate, Error> {
        let issued = control.check()?;
        let deadline = control.deadline(Duration::from_secs(2))?;
        let frame = self.next.ok_or(Error::InvalidFrame)?;
        // Retire before possible external work. Failed/cancelled exchanges poison
        // the worker; neither candidate identity nor original timestamp is retried.
        self.next = frame.next();
        let mut operation = MediaOperation::new(&mut self.worker);
        let mut reply = operation
            .worker
            .request(
                &control.cx,
                if conditional {
                    Kind::CaptureIfChanged
                } else {
                    Kind::Capture
                },
                capture_payload(frame, issued.as_micros(), force_idr),
                deadline,
            )
            .await?;
        let content = loop {
            control.check()?;
            match reply.header.kind {
                Kind::Unit => {
                    let unit = parse_unit(reply.into_body(), &self.configuration.limits()?)?;
                    if unit.frame() != frame
                        || unit.config_generation() != self.configuration.generation
                        || unit.capture_micros() != issued.as_micros()
                    {
                        return Err(Error::InvalidFrame);
                    }
                    self.last_capture = Some(unit.frame());
                    break Content::Encoded(unit);
                }
                Kind::Unchanged if conditional && !force_idr => {
                    let proof = UnchangedCapture::decode(reply.body())?;
                    if proof.candidate != frame
                        || Some(proof.reference) != self.last_capture
                        || proof.observed_micros != issued.as_micros()
                    {
                        return Err(Error::InvalidFrame);
                    }
                    break Content::Unchanged(proof);
                }
                Kind::NeedInput => {
                    let now = control
                        .cx
                        .timer_driver()
                        .ok_or(worker::Error::MissingRuntime)?
                        .now();
                    sleep(now, Duration::from_millis(1)).await;
                    reply = operation
                        .worker
                        .request(&control.cx, Kind::Poll, vec![], deadline)
                        .await?;
                }
                _ => return Err(Error::Backpressure),
            }
        };
        operation.completed = true;
        Ok(CaptureUpdate {
            source: self.source.clone(),
            configuration: self.configuration.generation,
            content,
        })
    }
}
impl Subscription {
    /// Bind actual encoded output to its worker before accepting that worker's
    /// unchanged-source observations. Reusing numeric frame/config IDs is not a
    /// source match. A missed changed frame cannot certify the older visible one.
    pub fn enqueue_capture(&mut self, update: CaptureUpdate) -> Result<(), Error> {
        let now = self.control.check()?;
        if update.configuration != self.epoch.configuration
            || self
                .capture_source
                .as_ref()
                .is_some_and(|source| !Arc::ptr_eq(source, &update.source))
        {
            return Err(Error::InvalidFrame);
        }
        match update.content {
            Content::Encoded(unit) => {
                self.enqueue(unit)?;
                self.capture_source = Some(update.source);
            }
            Content::Unchanged(proof) => {
                if self.capture_source.is_none() {
                    return Err(Error::InvalidFrame);
                }
                self.cache.observe_unchanged(
                    proof.reference.as_raw(),
                    proof.observed_micros,
                    now.as_micros(),
                )?;
            }
        }
        Ok(())
    }
}
