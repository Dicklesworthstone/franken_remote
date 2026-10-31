//! One fixed bootstrap acknowledgement after a terminal capability inspection.
//! This never restores negotiation, grants observation, or dispatches input.
use super::super::{Error, Host, Phase, RETIRED, Role, now};
use asupersync::time::timeout;
use fr_transport::quic::{self, Route};
use fr_wire::{inspection, negotiation};
use std::{sync::atomic::Ordering, time::Duration};

const DRAIN_US: u64 = 100_000;
const MAX_TURNS: usize = 16;

/// A recognized peer attempt may end without consuming the source factory.
/// Local owner, clock, configuration and resource failures retain their original
/// classification; they must not become permission to rearm a cold listener.
pub(super) fn terminal_error(error: Error) -> Error {
    use fr_wire::stream::StreamError;
    match error {
        Error::Cancelled
        | Error::Clock
        | Error::InvalidConfiguration
        | Error::Authority
        | Error::SharedPublication(_)
        | Error::PublicationRegistry(_)
        | Error::Protocol(negotiation::Error::Allocation)
        | Error::Transport(
            quic::Error::Cancelled
            | quic::Error::Clock
            | quic::Error::InvalidPolicy
            | quic::Error::Allocation
            | quic::Error::Stream(
                StreamError::ClockRegression | StreamError::InvalidPolicy | StreamError::Allocation,
            ),
        ) => error,
        _ => Error::InspectionAbandoned,
    }
}

pub(super) async fn report(host: &mut Host) -> Result<(), Error> {
    // Fence before the first await. No SelectedConfiguration was admitted and
    // no SessionOpened, consent notification, worker, or effect ever existed.
    host.phase = Phase::Closed;
    host.approval.store(RETIRED, Ordering::Release);
    if let Some(authority) = &mut host.authority {
        authority.close();
    }
    host.bytes.fill(0);
    host.len = 0;
    let result = send(host).await;
    host.close();
    result
}
async fn send(host: &mut Host) -> Result<(), Error> {
    let cx = &host.cx;
    let started = now(cx)?;
    if started < host.last {
        return Err(Error::Clock);
    }
    let peer = host.peer.as_ref().ok_or(Error::Closed)?;
    let until = started
        .checked_add(DRAIN_US)
        .ok_or(Error::Clock)?
        .min(host.until)
        .min(peer.check(cx, Role::Observe)?);
    let transport = host.transport.as_mut().ok_or(Error::Closed)?;
    peer.check_addresses(transport)?;
    let route = host.routes.outbound;
    if route.binding != 0 || route.messages != quic::Messages::Negotiation {
        return Err(Error::Order);
    }
    let mut bytes = [0; inspection::CLOSED_BYTES];
    let len =
        inspection::encode_closed(host.maximum, &mut bytes).map_err(negotiation::Error::from)?;
    let allowed = || {
        now(cx).is_ok_and(|at| at >= started && at < until) && peer.check(cx, Role::Observe).is_ok()
    };
    let remaining = until.checked_sub(now(cx)?).ok_or(Error::Expired)?;
    // Same connection and independent lifetime guard, with original pending
    // record deadlines. No admission refresh or application receive is possible.
    let drain = async {
        let mut queued = false;
        for _ in 0..MAX_TURNS {
            if !queued {
                match transport.send(cx, Route::Stream(route), &bytes[..len], until, allowed) {
                    Ok(()) => queued = true,
                    Err(quic::Error::Backpressure) => {}
                    Err(error) => return Err(error),
                }
            }
            transport
                .drive(cx, Duration::from_millis(10), allowed)
                .await?;
            if queued && transport.usage().retained_send_records == 0 {
                return Ok(());
            }
        }
        Err(quic::Error::Backpressure)
    };
    timeout(cx.now(), Duration::from_micros(remaining), drain)
        .await
        .map_err(|_| Error::Expired)?
        .map_err(Error::from)
}

#[cfg(test)]
mod tests;
