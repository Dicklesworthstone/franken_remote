//! Session-owned closure reporting; native teardown stays with its actual owners.
use super::{Error, HostSession};
use asupersync::cx::Cx;
use fr_wire::{
    authority::Binding,
    closure::{Cleanup, Closed, ClosedReason, OutstandingEffects},
    negotiation::Role,
};
use std::future::Future;

impl HostSession {
    /// End an observation-only session and attempt its final content-free report.
    /// This fences the original observation authority and stops renewal BEFORE
    /// returning, including when the returned future is never polled. The
    /// original transport is detached only into its existing terminal-only drain.
    ///
    /// `cleanup` is an independently provisioned cleanup context on the same
    /// runtime clock, not an un-cancelled copy of the session context. Its own
    /// cancellation, original ingress/credential gate and fixed 250-ms drain
    /// deadline still apply. The reason must describe the actual local ending.
    ///
    /// Cleanup is always Unconfirmed and effects Unknown: this owner cannot prove
    /// that a shared worker stopped or that all effect receipts were collected.
    /// The original publisher/native owner still performs ordered cleanup/reap.
    /// Control-intent sessions refuse reporting through this path and retain
    /// their existing lease-specific cleanup/reporting owner. No retry or delayed
    /// grant can reopen this session after a refused or abandoned report.
    pub fn close_with_report(
        &mut self,
        cleanup: &Cx,
        reason: ClosedReason,
    ) -> impl Future<Output = Result<(), Error>> + use<> {
        let observation_only = self.opened.selected.role == Role::Observe;
        self.opened.control.revoke();
        self.renewal.stop();
        let drain = observation_only.then(|| {
            self.opened.transport.close_with_closed(
                cleanup,
                &self.opened.connection,
                self.opened.routes.outbound,
                Binding {
                    channel: self.opened.binding.id,
                    session: self.opened.binding.remote_session,
                },
                Closed {
                    reason,
                    cleanup: Cleanup::Unconfirmed,
                    effects: OutstandingEffects::Unknown,
                },
            )
        });
        self.close();
        async move {
            match drain {
                Some(drain) => drain.await.map_err(Error::Transport),
                None => Err(Error::Order),
            }
        }
    }
}
