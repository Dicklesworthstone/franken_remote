//! Best-effort terminal metadata, never a second chance to negotiate or approve.
use super::super::{Error, Host, Phase, RETIRED, Role, now};
use asupersync::time::timeout;
use fr_transport::quic::{self, Route};
use fr_wire::{
    WireError,
    input::InputDelivery,
    negotiation,
    refusal::{self, Reason, Refused},
};
use std::{sync::atomic::Ordering, time::Duration};

const DRAIN_US: u64 = 100_000;
const MAX_TURNS: usize = 16;

fn reason(phase: Phase, observation_only: bool, error: Error) -> Option<Reason> {
    use negotiation::Error as Protocol;
    Some(match error {
        // Never answer a refusal with another refusal, or reopen dead I/O.
        Error::Protocol(Protocol::Refused(_))
        | Error::Transport(_)
        | Error::Cancelled
        | Error::Closed => return None,
        Error::Protocol(Protocol::Version | Protocol::Wire(WireError::UnsupportedVersion)) => {
            Reason::UnsupportedVersion
        }
        Error::Protocol(Protocol::Profile) => Reason::UnsupportedProfile,
        Error::Protocol(Protocol::RequiredCapability) => Reason::RequiredCapability,
        Error::Protocol(Protocol::Limits) => Reason::InvalidLimits,
        Error::Protocol(Protocol::Selection) => Reason::InvalidSelection,
        Error::Protocol(
            Protocol::Allocation
            | Protocol::Wire(WireError::ResourceLimit | WireError::ArithmeticOverflow),
        ) => Reason::ResourceLimit,
        Error::Protocol(_) => Reason::InvalidMessage,
        Error::Denied if phase == Phase::Hello && observation_only => Reason::ControlUnavailable,
        Error::Denied if phase == Phase::Approval => Reason::LocalApprovalDenied,
        Error::Expired if phase == Phase::Approval => Reason::ApprovalExpired,
        Error::Expired => Reason::Expired,
        Error::Order => Reason::InvalidState,
        Error::Admission(fr_tailnet::Error::TailnetMembershipUnverifiable) => {
            Reason::TailnetMembershipUnverifiable
        }
        Error::Denied
        | Error::Admission(
            fr_tailnet::Error::CapabilityDenied
            | fr_tailnet::Error::ScopeDenied
            | fr_tailnet::Error::MachineNotAuthorized
            | fr_tailnet::Error::LocalApiDenied
            | fr_tailnet::Error::ExplicitScopeRequired
            | fr_tailnet::Error::SharedPeer,
        ) => Reason::PermissionDenied,
        Error::Admission(fr_tailnet::Error::Expired | fr_tailnet::Error::KeyExpired) => {
            Reason::Expired
        }
        _ => Reason::HostUnavailable,
    })
}

fn fence(host: &mut Host) {
    host.phase = Phase::Closed;
    host.approval.store(RETIRED, Ordering::Release);
    if let Some(authority) = &mut host.authority {
        authority.close();
    }
    host.bytes.fill(0);
    host.len = 0;
}

pub(super) async fn report(host: &mut Host, error: Error) {
    if host.phase == Phase::Detached {
        return;
    }
    let report = matches!(
        host.phase,
        Phase::Hello | Phase::Selection | Phase::Approval
    )
    .then(|| reason(host.phase, host.observation_only, error))
    .flatten();
    // Fence BEFORE any await, including when reporting is impossible. These
    // phases have never enqueued SessionOpened, media, input or channel grants.
    // Later phases close without flushing potentially obsolete authority data.
    fence(host);
    if let Some(reason) = report {
        // Preserve the original local failure, not a secondary reporting error.
        let _ = send(host, reason).await;
    }
    host.close();
}

async fn send(host: &mut Host, reason: Reason) -> Result<(), Error> {
    let cx = &host.cx;
    let started = now(cx)?;
    if started < host.last {
        return Err(Error::Clock);
    }
    let peer = host.peer.as_ref().ok_or(Error::Closed)?;
    let until = started
        .checked_add(DRAIN_US)
        .ok_or(Error::Clock)?
        .min(peer.check(cx, Role::Observe)?);
    let transport = host.transport.as_mut().ok_or(Error::Closed)?;
    peer.check_addresses(transport)?;
    let route = host.routes.outbound;
    if route.binding != 0 || route.messages != quic::Messages::Negotiation {
        return Err(Error::Order);
    }
    let mut bytes = [0; refusal::MAX_BYTES];
    let len = refusal::encode(
        Refused::connection(reason),
        0,
        host.maximum,
        &mut bytes,
        InputDelivery::Reliable,
    )
    .map_err(|error| Error::Protocol(error.into()))?;
    let allowed = || {
        now(cx).is_ok_and(|at| at >= started && at < until) && peer.check(cx, Role::Observe).is_ok()
    };
    let remaining = until.checked_sub(now(cx)?).ok_or(Error::Expired)?;
    // The transport's independent lifetime guard and all old send deadlines
    // remain intact. No LocalAPI refresh, new permission, receive dispatch or
    // alternate socket is available inside this terminal send-only scope.
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
    // poll_io checks deadlines at every I/O poll but cannot itself wake an idle
    // socket. This outer timer also bounds a parked flush, not just loop turns.
    timeout(cx.now(), Duration::from_micros(remaining), drain)
        .await
        .map_err(|_| Error::Expired)?
        .map_err(Error::from)
}

#[cfg(test)]
mod tests;
