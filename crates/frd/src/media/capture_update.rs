//! Actual worker results with one source identity, not caller-made idle proofs.
use super::{
    CaptureSource, EncodedAccessUnit, Error, FrameId, MediaOperation, ObservationControl,
    Subscription, worker,
};
use asupersync::{time::sleep, types::Time};
use fr_core::ids::CodecConfigurationGeneration;
use fr_media::{
    delivery::SendError,
    worker::{Kind, UnchangedCapture},
};
use std::{fmt, sync::Arc, time::Duration};

mod protocol;
use protocol::Content;

mod prepared;
pub use prepared::PreparedSharedCapture;
mod shared;
pub use shared::{FanoutReport, SharedCaptureUpdate};

/// A complete worker result. Only `CaptureSource` can construct this proof. It
/// owns either one encoded buffer or fixed metadata; no raw capture escapes the
/// worker. Taking encoded bytes out deliberately loses source provenance.
pub struct CaptureUpdate {
    source: Arc<()>,
    configuration: CodecConfigurationGeneration,
    content: Content,
    applied_bitrate: Option<u32>,
}
/// A subscriber's recovery budget may bound its private worker, but must not
/// terminate an independent shared source still serving healthy subscriptions.
enum CaptureScope {
    Private,
    Shared {
        maximum_capacity: usize,
        join_until: Option<u64>,
    },
}
impl CaptureScope {
    fn limits(self) -> (Option<usize>, Option<u64>, bool) {
        match self {
            Self::Private => (None, None, false),
            Self::Shared {
                maximum_capacity,
                join_until,
            } => (Some(maximum_capacity), join_until, true),
        }
    }
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
    /// Worker-confirmed rate, absent on legacy operations. A requested rate
    /// alone is never evidence that the encoder changed its operating point.
    pub(crate) const fn applied_bitrate(&self) -> Option<u32> {
        self.applied_bitrate
    }
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
            .capture_request(control, force_idr, false, CaptureScope::Private, None)
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
        self.capture_request(control, force_idr, true, CaptureScope::Private, None)
            .await
    }
    /// Same original capture credit and immutable native deadline, with an
    /// explicit software rate target. An unchanged result may defer the target.
    pub(crate) async fn capture_at_rate(
        &mut self,
        control: &ObservationControl,
        force_idr: bool,
        bitrate: u32,
    ) -> Result<CaptureUpdate, Error> {
        self.capture_request(
            control,
            force_idr,
            true,
            CaptureScope::Private,
            Some(bitrate),
        )
        .await
    }
    async fn capture_request(
        &mut self,
        control: &ObservationControl,
        force_idr: bool,
        conditional: bool,
        scope: CaptureScope,
        bitrate: Option<u32>,
    ) -> Result<CaptureUpdate, Error> {
        protocol::validate_bitrate(self.configuration, bitrate)?;
        #[cfg(target_os = "linux")]
        let mut selection = super::discovery::SelectionGuard::capture(self, control)?;
        let issued = control.check()?;
        let (maximum_capacity, join_until, shared) = scope.limits();
        // Service requests on the original capture loop, not in a parallel
        // worker or per-viewer encoder. An expired cohort cannot reset healthy
        // capture; those failed senders retain their own terminal deadlines.
        let (scheduled_idr, recovery_until) = self
            .recovery
            .admit_capture(issued.as_micros(), join_until, shared)
            .map_err(Error::Receiver)?;
        let force_idr = force_idr || scheduled_idr;
        // Work keeps one immutable ceiling. Timely renewals of the original
        // authority are checked during IPC, not frozen at this first sample.
        let mut deadline = worker::Deadline::after(&control.cx, Duration::from_secs(2))?;
        if let Some(until) = recovery_until {
            let nanos = until.checked_mul(1000).ok_or(worker::Error::Deadline)?;
            deadline = deadline.capped_at(Time::from_nanos(nanos));
        }
        let frame = self.next.ok_or(Error::InvalidFrame)?;
        // Retire before possible external work. Failed/cancelled exchanges poison
        // the worker; neither candidate identity nor original timestamp is retried.
        self.next = frame.next();
        let request = protocol::Request {
            configuration: self.configuration,
            frame,
            observed_micros: issued.as_micros(),
            force_idr,
            conditional,
            bitrate,
            reference: self.last_capture,
        };
        let (kind, payload) = request.payload()?;
        let mut operation = MediaOperation::new(&mut self.worker);
        let mut reply = operation
            .worker
            .request_checked(
                &control.cx,
                kind,
                payload,
                maximum_capacity,
                deadline,
                || control.live_deadline(),
            )
            .await?;
        let (content, applied_bitrate) = loop {
            control.check()?;
            // Parsing/polling cannot restart the original recovery attempt.
            if control
                .cx
                .timer_driver()
                .ok_or(worker::Error::MissingRuntime)?
                .now()
                >= deadline.time()
            {
                return Err(worker::Error::Deadline.into());
            }
            if let Some((content, applied_bitrate)) = request
                .response(reply.header.kind, reply.into_body())
                .map_err(response_error)?
            {
                if let Content::Encoded(unit) = &content {
                    self.last_capture = Some(unit.frame());
                }
                break (content, applied_bitrate);
            }
            let now = control
                .cx
                .timer_driver()
                .ok_or(worker::Error::MissingRuntime)?
                .now();
            sleep(now, Duration::from_millis(1)).await;
            reply = operation
                .worker
                .request_checked(
                    &control.cx,
                    Kind::Poll,
                    vec![],
                    maximum_capacity,
                    deadline,
                    || control.live_deadline(),
                )
                .await?;
        };
        operation.completed = true;
        #[cfg(target_os = "linux")]
        selection.finish();
        Ok(CaptureUpdate {
            source: self.source.clone(),
            configuration: self.configuration.generation,
            content,
            applied_bitrate,
        })
    }
}
fn response_error(error: protocol::Error) -> Error {
    match error {
        protocol::Error::Protocol(error) => error.into(),
        protocol::Error::InvalidFrame => Error::InvalidFrame,
        protocol::Error::Backpressure => Error::Backpressure,
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
                self.admit_unchanged(&proof, now.as_micros())?;
            }
        }
        Ok(())
    }
    /// An observation already past the reference horizon on arrival (a stalled
    /// capture) is obsolete evidence, like queued metadata that expires: it is
    /// dropped with no effect. The view stays as stale as it is; only a later
    /// observation can refresh it, and a suspended lease survives (plan 11.3).
    fn admit_unchanged(&mut self, proof: &UnchangedCapture, now: u64) -> Result<(), Error> {
        match self
            .cache
            .observe_unchanged(proof.reference.as_raw(), proof.observed_micros, now)
        {
            Ok(_) | Err(SendError::ObservationExpired) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}
