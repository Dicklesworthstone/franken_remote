//! Carry original application negotiation and local approval into the same slot.
use super::{Admission, Error, Gate, State, Ticket};
use crate::session_startup::{Approval, Host, Role};
use fr_transport::quic::Disposition;
use std::time::Duration;

impl Admission {
    /// Accept a fresh Host from the existing TLS/ALPN and tailnet-admission
    /// boundary, BEFORE application negotiation or local approval. This does not
    /// bind a socket or establish ingress/identity; use `Host::from_admitted` with
    /// the actual protected connection and its original dedicated session Cx.
    ///
    /// Negotiation, approval, display choice and media attachment occupy ONE of
    /// the same bounded viewer slots immediately. The original Host deadline
    /// includes parked time, local consent and attachment; it is never reset.
    /// The application role is restricted to Observe before `ClientHello` admission.
    /// notify is a bounded nonblocking LOCAL approval notification, not a grant.
    /// It runs once, outside all registry/source locks, only when Host requests
    /// approval. Return success for notification, not for inferred permission.
    ///
    /// The original admission refresh/UDP owners continue while consent waits.
    /// Closing the ticket, source or hub cancels the original attempt, including
    /// an unpolled one, and cannot affect a later reused slot/session number.
    pub fn admit_host<F>(&self, mut host: Host, mut notify: F) -> Result<Ticket, Error>
    where
        F: FnMut(Approval, Role) -> Result<(), ()> + Send + 'static,
    {
        let (cx, parent, until) = host
            .bind_shared_source(self.source.clone())
            .map_err(Error::Session)?;
        let reservation = self.reserve(
            parent,
            Gate::Opening {
                cx: cx.clone(),
                until,
            },
            State::Opening,
        )?;
        let receipt = reservation.receipt.clone();
        let policy = reservation.policy;
        // Every handshake/native I/O and admission-refresh poll checks this
        // source too. Do not consume/replace the transport's ingress-check slot.
        let source = self.source.clone();
        let notify_source = source.clone();
        let opening = host.open(Duration::from_millis(5), move |approval, role| {
            notify_source.check_source().map_err(|_| ())?;
            notify(approval, role)?;
            notify_source.check_source().map_err(|_| ())?;
            Ok(())
        });
        let entropy = self.entropy.clone();
        let running_receipt = receipt.clone();
        let task = Box::pin(async move {
            let mut session = opening.await.map_err(Error::Session)?;
            session.check().map_err(Error::Session)?;
            source.check_source().map_err(Error::Source)?;
            if session.selection().role != Role::Observe {
                return Err(Error::WrongRole);
            }
            running_receipt.opened(session.original_observation())?;
            let join_entropy = entropy.clone();
            let mut shared = session
                .join_shared_display_capped(
                    source,
                    policy.join_timeout,
                    until,
                    policy.send,
                    move || join_entropy(),
                )
                .await
                .map_err(Error::Publication)?;
            running_receipt.serving();
            shared
                .serve(move || entropy(), |_, _| Ok(Disposition::Blocked))
                .await
                .map_err(Error::Session)
        });
        reservation.install(task)?;
        Ok(Ticket {
            receipt,
            registry: self.registry.clone(),
        })
    }
}
