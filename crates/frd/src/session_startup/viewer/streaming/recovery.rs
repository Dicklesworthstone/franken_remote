//! Recovery reports in the canonical observation loop, not UI callbacks.
use super::{Error, Peer, Repair, now, recovery_control};
use asupersync::cx::Cx;
use fr_media::delivery::{DeliveryError, ReceivePipeline};

pub(super) fn recoverable(error: DeliveryError) -> bool {
    matches!(
        error,
        DeliveryError::ReferenceExpired
            | DeliveryError::RecoveryExpired
            | DeliveryError::DecodeFailed
    )
}

/// Fences the real receiver before any new decode or transport work. There is
/// no control bypass here: an input-owning or control-requesting peer retains
/// the existing terminal refusal. Observation-only peers can report loss while
/// their original renewal owner remains active, bounded by the failed chain's
/// single absolute recovery deadline. No new binding or authority is invented.
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
    if !matches!(peer, Peer::Observe { .. }) {
        receiver
            .tick(now(cx).map_err(Error::Session)?)
            .map_err(Error::Delivery)?;
        return Ok(false);
    }
    let (session, _) = peer.parts()?;
    let until = session.heard_until;
    let state = report
        .service(cx, &mut session.transport, receiver, || {
            now(cx).is_ok_and(|current| current < until)
        })
        .map_err(Error::Recovery)?;
    let recovering = matches!(
        state,
        recovery_control::State::Pending | recovery_control::State::Requested
    );
    if recovering {
        repair.clear();
    }
    Ok(recovering)
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
        Err(Error::Delivery(error))
            if report.is_some() && matches!(peer, Peer::Observe { .. }) && recoverable(error) =>
        {
            service(peer, report, receiver, repair, cx)?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}
