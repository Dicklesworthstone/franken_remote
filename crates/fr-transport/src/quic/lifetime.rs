//! A connection-wide destination/ingress guard cannot be replaced by a later
//! application caller's authorization closure. This adds no peer permission.
pub(super) mod terminal;

use super::{Error, QuicRecords, attachment::Reservation, now};
use asupersync::cx::Cx;
use std::{
    future::{Future, poll_fn},
    pin::pin,
    sync::Arc,
    task::Poll,
};

impl QuicRecords {
    /// Install exactly once, before application traffic. Every later record
    /// admission, receive dispatch and native I/O poll also checks this guard.
    /// The callback must be bounded and nonblocking; do not do `LocalAPI` I/O here.
    /// A failed guard permanently closes this owner, never a replacement socket.
    pub fn retain_lifetime_check(
        &mut self,
        cx: &Cx,
        check: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<(), Error> {
        self.retain_lifetime_checks(cx, check.clone(), check)
    }

    /// Install both immutable lifetime gates BEFORE any application traffic.
    /// The ordinary gate includes the listener/session stop. The terminal gate
    /// MUST retain the SAME credential, endpoint, ingress and policy evidence,
    /// checked on the original independently provisioned broker/cleanup clock;
    /// it may omit only the already-fenced application's cancellation signal.
    /// Never pass an unconditional true to bypass expired security evidence.
    ///
    /// Both gates are frozen together. They cannot be replaced at revocation or
    /// attached after a single-gate installation. A terminal gate grants no
    /// ordinary I/O: only a detached one-record drain may invoke it instead.
    pub fn retain_lifetime_checks(
        &mut self,
        cx: &Cx,
        check: Arc<dyn Fn() -> bool + Send + Sync>,
        terminal: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<(), Error> {
        if self.lifetime_check.is_some() || self.terminal_lifetime_check.is_some() {
            return Err(Error::InvalidPolicy);
        }
        self.check(cx, &mut || true)?;
        if self.senders.iter().any(|s| s.bytes != 0) || self.read_bytes != 0 {
            return Err(Error::InvalidPolicy);
        }
        if !terminal() {
            self.close();
            return Err(Error::Unauthorized);
        }
        self.lifetime_check = Some(check);
        self.terminal_lifetime_check = Some(terminal);
        self.check(cx, &mut || true).map(|_| ())
    }
}

/// Immutable entry evidence from the exclusive native owner. Queued FRD0
/// records remain outside that borrow; only this proof can permit an optional
/// retirement to wait for the same native operation to finish.
pub(super) struct IoBoundary<'a> {
    pub(super) attachments: &'a [Reservation],
    pub(super) payload_empty: bool,
}

/// Terminal drains retain their original conservative guard. They have no
/// Held entry witness and cannot opt into ordinary payload-empty continuation.
pub(super) fn poll_io<T, E>(
    cx: &Cx,
    gate: Option<&(dyn Fn() -> bool + Send + Sync)>,
    authorize: &mut impl FnMut() -> bool,
    attachments: &[Reservation],
    started: u64,
    until: Option<u64>,
    io: impl Future<Output = Result<T, E>>,
) -> impl Future<Output = Result<T, Error>> {
    poll_io_at_boundary(
        cx,
        gate,
        authorize,
        IoBoundary {
            attachments,
            payload_empty: false,
        },
        started,
        until,
        io,
    )
}

pub(super) async fn poll_io_at_boundary<T, E>(
    cx: &Cx,
    gate: Option<&(dyn Fn() -> bool + Send + Sync)>,
    authorize: &mut impl FnMut() -> bool,
    boundary: IoBoundary<'_>,
    started: u64,
    until: Option<u64>,
    io: impl Future<Output = Result<T, E>>,
) -> Result<T, Error> {
    let mut io = pin!(io);
    poll_fn(|task| {
        if cx.checkpoint().is_err() {
            return Poll::Ready(Err(Error::Cancelled));
        }
        if gate.is_some_and(|check| !check()) || !authorize() {
            return Poll::Ready(Err(Error::Unauthorized));
        }
        let at = match now(cx) {
            Err(error) => return Poll::Ready(Err(error)),
            Ok(at) if at < started => return Poll::Ready(Err(Error::Clock)),
            Ok(at) if until.is_some_and(|end| at >= end) => {
                return Poll::Ready(Err(Error::Expired));
            }
            Ok(at) => at,
        };
        if boundary.attachments.iter().any(|r| r.expired(at)) {
            return Poll::Ready(Err(Error::Expired));
        }
        if !boundary.payload_empty
            && boundary
                .attachments
                .iter()
                .any(Reservation::retirement_pending)
        {
            // check() cannot retire streams while this future owns the native
            // connection. Its private protected packets cannot be selectively
            // retracted, so do not poll or resume it after a late retirement.
            // Held returns custody only to close, never another ordinary turn.
            return Poll::Ready(Err(Error::OptionalRetirementDuringIo));
        }
        // An operation entered with no native application payload or uncertain
        // protected output can finish under the SAME authority/deadline gates.
        // Its exclusive borrow cannot stage queued FRD0 records; peer traffic
        // can generate transport replies but no application bytes. Completion
        // returns custody to check(), which retires the pair before subsequent
        // application staging or dispatch. Never drop this pending future to
        // manufacture completion or substitute a post-return emptiness check.
        io.as_mut()
            .poll(task)
            .map(|result| result.map_err(|_| Error::Native))
    })
    .await
}
