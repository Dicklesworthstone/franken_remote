//! Recovery reports in the canonical observation loop, not UI callbacks.
use super::{Error, Peer, Repair, media, now, recovery_control};
use asupersync::cx::Cx;
use fr_media::delivery::{DeliveryError, ReceivePipeline};

pub(super) fn recoverable(error: DeliveryError) -> bool {
    matches!(
        error,
        DeliveryError::ReferenceExpired
            | DeliveryError::ReferenceCapacityExceeded
            | DeliveryError::RecoveryExpired
            | DeliveryError::DecodeFailed
    )
}

/// A positively negotiated observation session or control grant retains its
/// decoder across a failed chain; a grant's input stays suspended meanwhile
/// (plan 11.3, 12.3). A control-capable viewer without a request is still
/// observing. An explicit pending request is never rewritten by recovery.
pub(super) fn enabled(peer: &Peer, report: Option<&recovery_control::Receiver>) -> bool {
    report.is_some()
        && match peer {
            Peer::Observe { .. } | Peer::Control(_) => true,
            Peer::Viewing { viewing, .. } => viewing.requested.is_none(),
            Peer::Acquiring { .. } | Peer::Closed => false,
        }
}

/// Fences the real receiver before any new decode or transport work. A peer
/// still acquiring control retains the terminal refusal. Observers and control
/// grants report loss while their original renewal owner remains active,
/// bounded by the failed chain's single absolute recovery deadline. No new
/// binding or authority is invented, and a grant's input is suspended.
pub(super) fn service(
    peer: &mut Peer,
    report: Option<&mut recovery_control::Receiver>,
    receiver: &mut ReceivePipeline,
    repair: &mut Repair,
    cx: &Cx,
) -> Result<bool, Error> {
    let Some(report) = report else {
        receiver
            .tick(now(cx).map_err(Error::Session)?)
            .map_err(Error::Delivery)?;
        return Ok(false);
    };
    if !enabled(peer, Some(report)) {
        receiver
            .tick(now(cx).map_err(Error::Session)?)
            .map_err(Error::Delivery)?;
        return Ok(false);
    }
    let session = peer.parent()?;
    let until = session.heard_until;
    let state = report
        .service(cx, &mut session.transport, receiver, || {
            now(cx).is_ok_and(|current| current < until)
        })
        .map_err(|error| service_failure(error, receiver.failure()))?;
    let recovering = matches!(
        state,
        recovery_control::State::Pending | recovery_control::State::Requested
    );
    if recovering {
        repair.clear();
        if let Peer::Viewing {
            session, viewing, ..
        } = peer
        {
            viewing.begin_recovery(session)?;
        }
    }
    Ok(recovering)
}

/// Preserve a horizon actually exhausted by this original receiver's service,
/// after its connection/authority checks. Without negotiation, tick() already
/// returns the same Delivery error. Adding the reporting owner must not hide it
/// and suppress bounded fresh viewing after cleanup. The ORIGINAL receiver must
/// independently retain an expired horizon: a later recovery timeout cannot
/// relabel a decode failure, capacity fault or unknown cause as frame loss.
/// This never resumes the failed chain or renews any deadline. Other wrappers
/// retain their exact cause, including cancellation and authority failures.
fn service_failure(error: recovery_control::Error, original: Option<DeliveryError>) -> Error {
    match error {
        recovery_control::Error::Delivery(
            error @ (DeliveryError::ReferenceExpired | DeliveryError::RecoveryExpired),
        ) if matches!(
            original,
            Some(DeliveryError::ReferenceExpired | DeliveryError::RecoveryExpired)
        ) =>
        {
            Error::Delivery(error)
        }
        error => Error::Recovery(error),
    }
}

/// Late old-generation payloads are obsolete, not decoder input. The transport
/// still enforces their bound, role, stream framing and byte accounting.
pub(super) fn receive(
    receiver: &mut ReceivePipeline,
    channel: fr_wire::Channel,
    bytes: &[u8],
    current: u64,
    enabled: bool,
) -> Result<(), DeliveryError> {
    if enabled && receiver.state() == fr_media::delivery::ReceiveState::NeedsRecovery {
        return Ok(());
    }
    match receiver.receive(channel, bytes, current) {
        Ok(_) => Ok(()),
        Err(error) if enabled && recoverable(error) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Expiry may occur between two clock reads in the same bounded turn. Repair
/// preparation follows the exact same failure path instead of losing that race
/// and cancelling a reference-recovery-capable observation session.
pub(super) fn prepare(
    peer: &mut Peer,
    mut report: Option<&mut recovery_control::Receiver>,
    receiver: &mut ReceivePipeline,
    repair: &mut Repair,
    cx: &Cx,
) -> Result<(), Error> {
    if service(peer, report.as_deref_mut(), receiver, repair, cx)? {
        return Ok(());
    }
    match repair.prepare(receiver, now(cx).map_err(Error::Session)?) {
        Ok(()) => Ok(()),
        Err(Error::Delivery(error)) if enabled(peer, report.as_deref()) && recoverable(error) => {
            service(peer, report, receiver, repair, cx)?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Guard transitions which can discover expiry on their own clock read: taking
/// a queued picture and accepting a completed native job use the same ORIGINAL
/// recovery owner. None means no result can be used from this failed chain;
/// neither selection nor native completion can restart its deadline.
///
/// A completion closure owns its native job. Skipping/dropping it retires the
/// still-charged bytes, never another receiver's reservation or native reply.
/// Protocol errors, foreign scopes, cancellation and control remain terminal.
pub(super) fn admit<T>(
    peer: &mut Peer,
    mut report: Option<&mut recovery_control::Receiver>,
    receiver: &mut ReceivePipeline,
    repair: &mut Repair,
    cx: &Cx,
    operation: impl FnOnce(&mut ReceivePipeline) -> Result<T, media::Error>,
) -> Result<Option<T>, Error> {
    if service(peer, report.as_deref_mut(), receiver, repair, cx)? {
        return Ok(None);
    }
    match operation(receiver) {
        Ok(value) => Ok(Some(value)),
        Err(media::Error::Receiver(error))
            if enabled(peer, report.as_deref()) && recoverable(error) =>
        {
            if service(peer, report, receiver, repair, cx)? {
                Ok(None)
            } else {
                Err(Error::Media(media::Error::Receiver(error)))
            }
        }
        Err(error) => Err(Error::Media(error)),
    }
}

#[cfg(test)]
mod capacity_tests;
