//! Terminal send-only scope for a metadata inspection that never acquired input
//! or media channels. Do not reuse this drain for an active desktop: it could
//! otherwise flush queued input after the user requested a stop.
use super::{Budget, Error, ViewerSession};
use crate::session_startup::now;
use asupersync::time::timeout;
use fr_transport::quic::{self, Route};
use fr_wire::{
    authority::Binding,
    closure::{self, CloseRequest, Reason},
    input::{InputDelivery, InputDirection},
};
use std::time::Duration;

const DRAIN_US: u64 = 100_000;
const MAX_TURNS: usize = 16;

/// Preserve the successfully received catalog even if this best-effort notice
/// cannot arrive. Consuming the session makes cancellation/unpolled drop terminal
/// through its existing Drop implementation. No remote cleanup receipt is made.
pub(super) async fn finish(mut session: ViewerSession, budget: &Budget) {
    if let Ok((bytes, started, until)) = prepare(&mut session, budget) {
        let cx = session.cx.clone();
        if let Ok(current) = now(&cx)
            && let Some(remaining) = until.checked_sub(current).filter(|n| *n != 0)
        {
            // The original destination lifetime guard, all retained-send
            // deadlines and the inspection's original budget remain in force.
            let _ = timeout(
                cx.now(),
                Duration::from_micros(remaining),
                drain(&mut session, &bytes, started, until),
            )
            .await;
        }
    }
    session.close();
}
fn prepare(
    session: &mut ViewerSession,
    budget: &Budget,
) -> Result<([u8; closure::REQUEST_BYTES], u64, u64), Error> {
    session.check().map_err(Error::Session)?;
    budget.remaining()?;
    let started = now(&session.cx).map_err(Error::Session)?;
    let until = started
        .checked_add(DRAIN_US)
        .ok_or(Error::Expired)?
        .min(budget.until)
        .min(session.heard_until);
    if until <= started {
        return Err(Error::Expired);
    }
    let mut bytes = [0; closure::REQUEST_BYTES];
    closure::encode_request(
        CloseRequest {
            reason: Reason::InspectionComplete,
        },
        Binding {
            channel: session.opened.binding.id,
            session: session.opened.binding.remote_session,
        },
        &session.opened.selection.limits,
        &mut bytes,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .map_err(Error::Wire)?;
    // No more observation renewal, clock service, catalog dispatch or channel
    // admission can occur. Cancellation still closes the original transport.
    session.closed = true;
    session.responder.stop();
    if let Some(clock) = &mut session.clock {
        clock.stop();
    }
    Ok((bytes, started, until))
}
async fn drain(
    session: &mut ViewerSession,
    bytes: &[u8],
    started: u64,
    until: u64,
) -> Result<(), quic::Error> {
    let cx = &session.cx;
    let route = session.routes.outbound;
    let allowed = || now(cx).is_ok_and(|at| at >= started && at < until);
    let mut queued = false;
    for _ in 0..MAX_TURNS {
        if !queued {
            match session
                .transport
                .send(cx, Route::Stream(route), bytes, until, allowed)
            {
                Ok(()) => queued = true,
                Err(quic::Error::Backpressure) => {}
                Err(error) => return Err(error),
            }
        }
        session
            .transport
            .drive(cx, Duration::from_millis(10), allowed)
            .await?;
        if queued && session.transport.usage().retained_send_records == 0 {
            // Transport retention cleared, not confirmation of native cleanup.
            return Ok(());
        }
    }
    Err(quic::Error::Backpressure)
}

#[cfg(test)]
mod tests;
