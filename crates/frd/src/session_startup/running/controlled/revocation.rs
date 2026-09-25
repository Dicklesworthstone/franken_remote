//! Terminal reporting owns no input authority and never waits for native cleanup.
use super::{ControlledHost, StopReason};
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
