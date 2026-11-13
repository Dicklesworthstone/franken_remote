//! A connection-wide destination/ingress guard cannot be replaced by a later
//! application caller's authorization closure. This adds no peer permission.
pub(super) mod terminal;

use super::{Error, QuicRecords, attachment::Reservation, now};
use asupersync::cx::Cx;
use std::{
    future::{Future, poll_fn},
    pin::{Pin, pin},
    sync::Arc,
    task::Poll,
};

impl QuicRecords {
    /// Retain one additional application-operation guard on this exact owner.
    /// Optional media may attach after startup, so registration is allowed
    /// after application traffic. This guard can only narrow the immutable
    /// connection lifetime: neither guard can be replaced or removed, and a
    /// per-call authorization callback cannot bypass either one.
    ///
    /// The callback runs before every record admission, receive dispatch and
    /// native I/O poll. It must be bounded and nonblocking. Keep original send
    /// permits/deadlines until actual drain evidence releases their custody;
    /// returning false closes this connection before any further ordinary I/O.
    /// A drained optional lane can return true to preserve its parent control.
    /// Terminal reports still require their independent empty-payload proof
    /// and original terminal security gate; this installs no cleanup authority.
    pub fn retain_operation_check(
        &mut self,
        cx: &Cx,
        check: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<(), Error> {
        if self.operation_check.is_some() {
            return Err(Error::InvalidPolicy);
        }
        self.check(cx, &mut || true)?;
        self.operation_check = Some(check);
        self.check(cx, &mut || true).map(|_| ())
    }

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

/// One drive turn under its bound, judged only AFTER the turn has been polled.
///
/// `asupersync::time::timeout` fails a future as soon as its deadline has
/// passed, before polling it again. On an oversubscribed host the thread can
/// be away from a turn for longer than the bound between two of its polls
/// (preempted, or inside another task's long poll) while the turn's I/O
/// finished; that turn failed, closing the connection, without being looked
/// at. Measured on a loaded macOS host: one 252 ms hub poll, then
/// a turn with a 10 ms receive wait "expired" after 259 ms, ending sessions
/// after 5-36 s. Here the turn gets its poll first: if it can finish now, it
/// finishes. The bound still holds: once `bound` is ready, the first poll that
/// leaves the turn pending fails it with `Error::Expired` and drops it exactly
/// as the timeout did (`Held` makes that terminal), so a turn never runs more
/// than one poll past its bound.
///
/// Both futures stay pinned in the caller: holding them by value here would
/// store the large turn future twice in the caller's state.
pub(super) fn within_turn_bound<'a, T>(
    mut turn: Pin<&'a mut impl Future<Output = Result<T, Error>>>,
    mut bound: Pin<&'a mut impl Future<Output = ()>>,
) -> impl Future<Output = Result<T, Error>> + 'a {
    poll_fn(move |task| {
        if let Poll::Ready(result) = turn.as_mut().poll(task) {
            return Poll::Ready(result);
        }
        bound.as_mut().poll(task).map(|()| Err(Error::Expired))
    })
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
pub(super) async fn poll_io<T, E>(
    cx: &Cx,
    gate: Option<&(dyn Fn() -> bool + Send + Sync)>,
    authorize: &mut impl FnMut() -> bool,
    attachments: &[Reservation],
    started: u64,
    until: Option<u64>,
    io: impl Future<Output = Result<T, E>>,
) -> Result<T, Error> {
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
        pin!(io),
    )
    .await
}

/// `io` stays pinned in the caller: taking it by value would store the
/// native driver future twice (the argument and its pinned copy) in every
/// turn's state, and every viewer future that awaits a turn.
pub(super) async fn poll_io_at_boundary<T, E>(
    cx: &Cx,
    gate: Option<&(dyn Fn() -> bool + Send + Sync)>,
    authorize: &mut impl FnMut() -> bool,
    boundary: IoBoundary<'_>,
    started: u64,
    until: Option<u64>,
    mut io: Pin<&mut impl Future<Output = Result<T, E>>>,
) -> Result<T, Error> {
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

#[cfg(test)]
mod turn_bound_tests;
