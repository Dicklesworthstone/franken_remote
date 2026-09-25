//! Remote cursor IPC on the ORIGINAL media workers (plan §11.4).
//!
//! The capture worker reports the host's logical cursor separately from the
//! captured pixels (which exclude the pointer); the presenter composites the
//! one client-rendered cursor. Neither exchange is capture freshness, decode,
//! presentation or input evidence: no frame ID, recovery schedule, source
//! progress or view readiness changes here.
use super::{CaptureSource, Error, MediaOperation, ObservationControl, Presenter};
use crate::worker;
use fr_media::worker::{Kind, Record, cursor, overlay};
use std::time::Duration;

/// A capture worker's typed `ReadCursor` reply. `None` records that the
/// pointer moved between the native image and position queries.
pub struct CursorReply(Option<Record>);
impl std::fmt::Debug for CursorReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "CursorReply(Observed)"
        } else {
            "CursorReply(Moving)"
        })
    }
}
impl CursorReply {
    /// Borrow the reply body as a typed observation, validated before use.
    pub fn observation(&self, source: &CaptureSource) -> Result<cursor::Observation<'_>, Error> {
        match &self.0 {
            None => Ok(cursor::Observation::Moving),
            Some(reply) => Ok(cursor::decode_observation(
                reply.body(),
                &source.configuration.limits()?,
            )?),
        }
    }
}
impl CaptureSource {
    /// One bounded `ReadCursor` exchange between captures, under the same
    /// selected-source consent. A failed or abandoned exchange poisons the
    /// worker exactly like a capture; typed cursor states never do.
    pub async fn read_cursor(
        &mut self,
        control: &ObservationControl,
    ) -> Result<CursorReply, Error> {
        control.check()?;
        if self
            .selected_control
            .as_ref()
            .is_some_and(|original| !original.same_owner(control))
        {
            return Err(Error::InvalidFrame);
        }
        let limits = self.configuration.limits()?;
        let capacity = cursor::MAX_BYTES.min(limits.max_control_message_bytes() as usize);
        let deadline = control.deadline(Duration::from_secs(1))?;
        let mut operation = MediaOperation::new(&mut self.worker);
        let reply = operation
            .worker
            .request_with_response_capacity(
                &control.cx,
                Kind::ReadCursor,
                vec![],
                Some(capacity),
                deadline,
            )
            .await?;
        operation.completed = true;
        control.check()?;
        match reply.header.kind {
            Kind::NeedInput => Ok(CursorReply(None)),
            Kind::CursorSnapshot => {
                // Validate now so a malformed body is a worker protocol error.
                cursor::decode_observation(reply.body(), &limits)?;
                Ok(CursorReply(Some(reply)))
            }
            _ => Err(Error::Worker(worker::Error::Protocol(
                fr_media::worker::Error::WrongState,
            ))),
        }
    }
}
impl Presenter {
    /// Install the client-rendered cursor on the presenter BETWEEN decode jobs.
    /// The worker re-presents its retained picture with the new overlay; this
    /// is not a decode completion, presentation receipt or visibility proof.
    pub(crate) async fn apply_cursor(
        &mut self,
        cx: &asupersync::cx::Cx,
        update: &overlay::Update<'_>,
    ) -> Result<(), Error> {
        if self.worker.state() != worker::State::Running {
            return Err(worker::Error::Unavailable.into());
        }
        let body = overlay::encode(update, &self.configuration.limits()?)?;
        let deadline = worker::Deadline::after(cx, Duration::from_millis(200))?;
        let mut operation = MediaOperation::new(&mut self.worker);
        let reply = operation
            .worker
            .request(cx, Kind::CursorOverlay, body, deadline)
            .await?;
        operation.completed = true;
        if reply.header.kind != Kind::CursorOverlayApplied {
            return Err(Error::InvalidFrame);
        }
        Ok(())
    }
}
