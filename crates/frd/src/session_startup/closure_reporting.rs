//! The listener owns final reporting outside the already-fenced application.
//! This owner never certifies another task's native cleanup or action receipts.
use super::{Error, Host, OpenedSession, Phase, Role};
use asupersync::cx::Cx;
use fr_transport::quic::{self, ClosedRegistration, ClosedReport};
use fr_wire::{
    authority::Binding,
    closure::{Cleanup, Closed, ClosedReason, OutstandingEffects},
};
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};

/// Outcome of one terminal transport attempt, not native cleanup confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationClosure {
    pub report: Closed,
    pub delivery: Result<(), quic::Error>,
}

pub(crate) struct Reporting {
    consumer: ClosedReport,
    reason: Arc<AtomicU8>,
}
pub(crate) struct Configuration {
    cleanup: Cx,
    registration: ClosedRegistration,
    reason: Arc<AtomicU8>,
}
impl Reporting {
    pub(crate) fn new(cleanup: &Cx) -> (Self, Configuration) {
        let consumer = ClosedReport::default();
        let reason = Arc::new(AtomicU8::new(0));
        let configuration = Configuration {
            cleanup: cleanup.clone(),
            registration: consumer.registration(),
            reason: reason.clone(),
        };
        (Self { consumer, reason }, configuration)
    }
    pub(crate) fn finish(
        self,
        fallback: ClosedReason,
    ) -> impl Future<Output = Option<ObservationClosure>> + use<> {
        let reason = recorded(self.reason.load(Ordering::Acquire), fallback);
        let report = Closed {
            reason,
            cleanup: Cleanup::Unconfirmed,
            effects: OutstandingEffects::Unknown,
        };
        let finish = self.consumer.finish(report);
        async move {
            finish
                .await
                .map(|delivery| ObservationClosure { report, delivery })
        }
    }
}
/// 1 and 6 are recorded only by the original close-request parser (a FIN,
/// cancelled task or generic `PeerClosed` is not a request); 7 only by the host
/// when this viewer's source could not be prepared. Anything else is fallback.
fn recorded(value: u8, fallback: ClosedReason) -> ClosedReason {
    match value {
        1 => ClosedReason::ClientRequested,
        6 => ClosedReason::ProtocolError,
        7 => ClosedReason::HostFailure,
        _ => fallback,
    }
}
impl Host {
    pub(crate) fn retain_observation_reporting(
        &mut self,
        configuration: Configuration,
    ) -> Result<(), Error> {
        if self.phase != Phase::Hello || self.observation_reporting.is_some() {
            return Err(Error::Order);
        }
        self.observation_reporting = Some(configuration);
        Ok(())
    }
}
impl Configuration {
    pub(super) fn arm(self, session: &mut OpenedSession) -> Result<(), Error> {
        // Negotiated role, not the host's maximum offer. Never occupy the
        // terminal slot needed by this connection's eventual granted lease.
        if session.selected.role != Role::Observe {
            return Ok(());
        }
        let control = session.control.clone();
        session
            .transport
            .arm_closed_report(
                &self.cleanup,
                &session.connection,
                session.routes.outbound,
                Binding {
                    channel: session.binding.id,
                    session: session.binding.remote_session,
                },
                self.registration,
                move || control.revoke(),
            )
            .map_err(Error::Transport)?;
        session.closure_reason = Some(self.reason);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_recorded_reasons_override_the_listener_fallback() {
        let fallback = ClosedReason::HostStopping;
        assert_eq!(recorded(1, fallback), ClosedReason::ClientRequested);
        assert_eq!(recorded(6, fallback), ClosedReason::ProtocolError);
        assert_eq!(recorded(7, fallback), ClosedReason::HostFailure);
        for unrecorded in [0, 2, 3, 4, 5, 8, 255] {
            assert_eq!(recorded(unrecorded, fallback), fallback);
        }
    }
}
