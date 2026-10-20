//! Telemetry usefulness is a prepared-record guard, never connection authority.
use super::{Cx, Error, QuicRecords, Route, quic};
use fr_media::receiver_feedback::{Admission, Error as FeedbackError};

pub(super) fn send(
    q: &mut QuicRecords,
    cx: &Cx,
    route: Route,
    bytes: &[u8],
    prepared_at: u64,
    until: u64,
) -> Result<Admission, Error> {
    send_with_clock(q, cx, route, bytes, prepared_at, until, || {
        cx.checkpoint().map_err(|_| Error::Closed)?;
        Ok(cx.timer_driver().ok_or(Error::Clock)?.now().as_nanos() / 1000)
    })
}

fn send_with_clock(
    q: &mut QuicRecords,
    cx: &Cx,
    route: Route,
    bytes: &[u8],
    prepared_at: u64,
    until: u64,
    mut now: impl FnMut() -> Result<u64, Error>,
) -> Result<Admission, Error> {
    let result = q.send_prepared(
        cx,
        route,
        bytes,
        until,
        || cx.checkpoint().is_ok(),
        || {
            let current = now()?;
            if current < prepared_at {
                return Err(Error::Clock);
            }
            if current >= until {
                return Err(Error::Load(FeedbackError::Expired));
            }
            Ok(())
        },
    );
    match result {
        Ok(quic::SendAdmission::Accepted) => Ok(Admission::Accepted),
        Ok(quic::SendAdmission::Refused(Error::Load(FeedbackError::Expired))) => {
            Ok(Admission::Expired)
        }
        Ok(quic::SendAdmission::Refused(error)) => Err(error),
        Err(quic::Error::Backpressure) => Ok(Admission::Backpressure),
        // Only pre-admission expiry leaves the connection open. Expired native
        // retransmission state, cancellation and actual authority loss remain
        // terminal and must not masquerade as a discarded telemetry sample.
        Err(quic::Error::Expired) if !q.is_closed() => Ok(Admission::Expired),
        Err(error) => Err(Error::Transport(error)),
    }
}

#[cfg(test)]
#[path = "admission/tests.rs"]
mod tests;
