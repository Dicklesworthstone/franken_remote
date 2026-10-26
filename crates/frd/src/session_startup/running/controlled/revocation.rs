//! Terminal reporting owns no input authority and never waits for native cleanup.
use super::{ControlledHost, HostSession, StopReason};
use crate::{input_quic::grant::GrantBroker, input_watchdog::Control};
use fr_core::ids::InputLeaseId;
use fr_transport::quic;
use fr_wire::{
    authority::Binding,
    lease_revoked::{CleanupStage, EffectStage, Reason, Revoked},
};
use std::future::Future;

fn wire_reason(reason: StopReason) -> Reason {
    match reason {
        StopReason::LocalRevoke => Reason::LocalRevoke,
        StopReason::ViewInvalidated => Reason::ViewInvalidated,
        StopReason::Suspended => Reason::Suspended,
        StopReason::ClockRegression
        | StopReason::ClockOverflow
        | StopReason::WatchdogDropped
        | StopReason::NativeFailure => Reason::HostFailure,
        // AuthorityEnded does not distinguish a lease timeout from observation
        // loss. Never guess the more specific LeaseExpired reason from it.
        StopReason::AuthorityEnded | StopReason::Cancelled | StopReason::ClientDisconnected => {
            Reason::SessionEnded
        }
    }
}

impl ControlledHost {
    /// Fence input immediately and attempt the original lease's terminal report.
    /// Native release proceeds through the independently driven input owner;
    /// neither a slow viewer nor report loss can delay the synchronous fence.
    ///
    /// This ends this remote session. It does not end the shared OS desktop or
    /// grant another controller. The transport refuses any unsafe native backlog
    /// rather than draining media under this terminal-only permission.
    ///
    /// Unknown effects remain unknown: callers may still collect the canonical
    /// native reply with `collect_after_close`. No input operation is retried.
    pub fn revoke_and_close(
        &mut self,
        requested: StopReason,
    ) -> impl Future<Output = Result<(), quic::Error>> + '_ {
        let control = self.input.control();
        control.stop(requested);
        let reason = control.reason().unwrap_or(requested);
        self.files.stop();
        if let Some(clipboard) = &self.clipboard {
            clipboard.stop();
        }
        self.clipboard_setup.stop();
        self.renewal.stop();
        let report = Revoked {
            lease: self.renewal.lease_id(),
            reason: wire_reason(reason),
            // A stop flag and a transport ACK are not native-release receipts.
            cleanup: CleanupStage::Fenced,
            effects: EffectStage::Unknown,
        };
        let binding = Binding {
            channel: self.session.opened.binding.id,
            session: self.session.opened.binding.remote_session,
        };
        let terminal = self.session.opened.transport.close_with_revocation(
            &self.session.opened.cx,
            self.renewal.original_connection(),
            self.session.opened.routes.outbound,
            binding,
            report,
        );
        // The ordinary transport is already closed. Do not cancel its Cx until
        // the terminal attempt ends: it is the same monotonic clock and retains
        // its independent cancellation and connection-lifetime checks.
        let closing = Closing(self);
        async move {
            let result = terminal.await;
            let _ = closing.0.terminal_report.get_or_insert(result);
            drop(closing);
            result
        }
    }

    /// `None` means no completed attempt (including an abandoned future). An ACK
    /// concerns report bytes only, never cleanup, rollback or peer UI handling.
    pub const fn revocation_delivery(&self) -> Option<Result<(), quic::Error>> {
        self.terminal_report
    }
}

struct Closing<'a>(&'a mut ControlledHost);
impl Drop for Closing<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Prepared by the enclosing service before borrowing its publisher. Neither
/// this weak registration nor the cleanup context grants input or observation.
pub(in crate::session_startup::running) struct Configuration {
    pub cleanup: asupersync::cx::Cx,
    pub registration: quic::RevocationRegistration,
    pub armed: bool,
}
/// The same session's reporting configuration accompanies the provisional
/// native owner, so a queued grant cannot leave before its lease fence owns
/// terminal reporting. No socket or input submission authority is moved here.
pub(in crate::session_startup::running) struct GrantReporting {
    configuration: Configuration,
    original: quic::ConnectionBinding,
    route: quic::StreamRoute,
    binding: Binding,
    previous: Option<quic::ClosedRegistration>,
}
impl GrantReporting {
    pub(in crate::session_startup::running) fn take(
        session: &mut HostSession,
    ) -> Result<Option<Self>, super::Error> {
        if session.opened.pregrant_reporting.is_some() && session.revocation_reporting.is_none() {
            return Err(super::Error::Order);
        }
        Ok(session
            .revocation_reporting
            .take()
            .map(|configuration| Self {
                configuration,
                original: session.opened.connection.clone(),
                route: session.opened.routes.outbound,
                binding: Binding {
                    channel: session.opened.binding.id,
                    session: session.opened.binding.remote_session,
                },
                previous: session.opened.pregrant_reporting.clone(),
            }))
    }
    pub(in crate::session_startup::running) fn queued(
        &mut self,
        broker: &mut GrantBroker,
        transport: &mut quic::QuicRecords,
    ) -> Result<(), crate::input_quic::grant::Error> {
        let (lease, control) = broker.queued_lease(transport)?;
        self.configuration
            .install(
                transport,
                &self.original,
                self.route,
                self.binding,
                lease,
                self.previous.as_ref(),
                control,
            )
            .map_err(crate::input_quic::grant::Error::Transport)
    }
    pub(in crate::session_startup::running) fn finish(self) -> Configuration {
        self.configuration
    }
}
impl Configuration {
    #[allow(clippy::too_many_arguments)]
    fn install(
        &mut self,
        transport: &mut quic::QuicRecords,
        original: &quic::ConnectionBinding,
        route: quic::StreamRoute,
        binding: Binding,
        lease: InputLeaseId,
        previous: Option<&quic::ClosedRegistration>,
        control: Control,
    ) -> Result<(), quic::Error> {
        if self.armed {
            return transport.confirm_revocation_report(
                original,
                route,
                binding,
                lease,
                previous,
                &self.registration,
            );
        }
        let fence = move || {
            // Preserve a watchdog/platform/local reason which already won.
            control.stop(StopReason::ClientDisconnected);
            wire_reason(control.reason().unwrap_or(StopReason::ClientDisconnected))
        };
        if let Some(previous) = previous {
            transport.promote_closed_report(
                original,
                route,
                binding,
                lease,
                previous,
                self.registration.clone(),
                fence,
            )
        } else {
            transport.arm_revocation_report(
                &self.cleanup,
                original,
                route,
                binding,
                lease,
                self.registration.clone(),
                fence,
            )
        }?;
        self.armed = true;
        Ok(())
    }
}
impl ControlledHost {
    pub(super) fn arm_revocation_reporting(
        &mut self,
        mut reporting: Configuration,
    ) -> Result<(), super::Error> {
        let opened = &mut self.session.opened;
        let binding = Binding {
            channel: opened.binding.id,
            session: opened.binding.remote_session,
        };
        reporting
            .install(
                &mut opened.transport,
                self.renewal.original_connection(),
                opened.routes.outbound,
                binding,
                self.renewal.lease_id(),
                opened.pregrant_reporting.as_ref(),
                self.input.control(),
            )
            .map_err(super::Error::Transport)?;
        opened.pregrant_reporting = None;
        self.terminal_registered = true;
        Ok(())
    }
}
