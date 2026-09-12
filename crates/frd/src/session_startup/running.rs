//! Persistent host service on the negotiated connection. Renew observation and
//! installed-Tailscale admission without stopping the UDP/TLS reactor during a
//! `LocalAPI` lookup. No listener, control grant or new media queue is created.
use super::{Error, OpenedSession, now};
use crate::media::{ObservationControl, renewal::ObservationRenewal};
use asupersync::cx::Cx;
use fr_core::time::HostInstant;
use fr_transport::quic::{ControlRoutes, Disposition, QuicRecords, Route};
use fr_wire::negotiation::{ControlBinding, Selection};
use std::{future::Future, pin::pin, task::Poll, time::Duration};

pub(super) mod controlled;
pub use controlled::ControlledHost;
pub(super) mod publisher;
mod streaming;
pub use streaming::StreamingHost;
#[cfg(test)]
pub(in crate::session_startup) use streaming::tests::feedback_pair;

const REFRESH_MARGIN_US: u64 = 500_000;
const MAX_TURN: Duration = Duration::from_millis(100);

/// Synchronous, bounded application work, serviced during admission refresh as
/// well as ordinary I/O. No native calls or blocking callbacks belong here.
mod input_wake;

trait Services {
    /// A new locally collected native submission, never packet receipt or renewal.
    fn input_submitted(&mut self, _at_us: u64) {}

    fn permitted(&mut self) -> bool {
        true
    }
    fn maintain<N: FnMut() -> Result<u128, ()>>(
        &mut self,
        _transport: &mut QuicRecords,
        _nonce: &mut N,
    ) -> Result<(), Error> {
        Ok(())
    }
    fn receive(&mut self, route: Route, bytes: &[u8]) -> Result<Disposition, ()>;
}
impl<F: FnMut(Route, &[u8]) -> Result<Disposition, ()>> Services for F {
    fn receive(&mut self, route: Route, bytes: &[u8]) -> Result<Disposition, ()> {
        self(route, bytes)
    }
}

/// Owns the actual admitted host session and its sole observation-renewal owner.
/// Drive this task during idle as well as active video. The callback only admits
/// bounded work to existing media/input owners; it must not block on native I/O.
/// Local lifecycle stops still use the shared observation and input controls.
pub struct HostSession {
    opened: OpenedSession,
    renewal: ObservationRenewal,
}
impl OpenedSession {
    /// Attach exactly one observation renewer after the binding acknowledgement.
    /// No new authority is minted and no existing expiration is extended here.
    pub fn into_running(mut self) -> Result<HostSession, Error> {
        self.check()?;
        let renewal = ObservationRenewal::new(
            self.control.clone(),
            &self.transport,
            self.routes,
            self.selected.limits,
        )
        .map_err(Error::Renewal)?;
        Ok(HostSession {
            opened: self,
            renewal,
        })
    }
}
impl HostSession {
    /// Publish a locally enumerated, approved disclosure scope and retain the
    /// resulting choice while media runs. Enumeration must not block this owner.
    /// Continue driving this session and dispatch display records between turns.
    pub fn select_display(
        &mut self,
        catalog: fr_wire::display::Catalog,
        timeout: Duration,
    ) -> Result<crate::display_selection::DisplaySelection, crate::display_selection::Error> {
        use crate::display_selection::{DisplaySelection, Error as DisplayError};
        self.check().map_err(|_| DisplayError::Closed)?;
        DisplaySelection::host(
            &mut self.opened.transport,
            fr_transport::quic::ChannelScope {
                control: self.opened.routes,
                parent: self.opened.binding,
                selection: &self.opened.selected,
            },
            self.opened.control.clone(),
            catalog,
            timeout,
        )
    }
    /// Join an initial-grant broker to this actual negotiated owner. Input routes
    /// must already be explicitly authenticated/installed on the same connection;
    /// this does not silently add input routes, consent or view readiness.
    pub fn control_broker(
        &mut self,
        seat: crate::input_agent::Seat,
        input: crate::input_quic::Routes,
    ) -> Result<crate::input_quic::grant::GrantBroker, crate::input_quic::grant::Error> {
        use crate::input_quic::grant::{Error as GrantError, GrantBroker, Scope};
        self.check().map_err(|_| GrantError::Stopped)?;
        self.opened
            .peer
            .check(&self.opened.cx, fr_wire::negotiation::Role::RequestControl)
            .map_err(|_| GrantError::NotNegotiated)?;
        GrantBroker::new(
            self.opened.control.clone(),
            &self.opened.transport,
            seat,
            Scope {
                parent: self.opened.binding,
                control: self.opened.routes,
                selection: &self.opened.selected,
            },
            input,
        )
    }
    /// Initial control over the completed input attachment, rather than caller-
    /// installed routes. The retained host identity, selection and observation
    /// are this running session's; neither channel negotiation nor a request
    /// supplies local consent, a ready view, or an already-granted lease.
    pub fn negotiated_control_broker(
        &mut self,
        seat: crate::input_agent::Seat,
        input: crate::input_quic::NegotiatedInput,
    ) -> Result<crate::input_quic::grant::GrantBroker, crate::input_quic::grant::Error> {
        use crate::input_quic::grant::{Error as GrantError, GrantBroker, Scope};
        self.check().map_err(|_| GrantError::Stopped)?;
        self.opened
            .peer
            .check(&self.opened.cx, fr_wire::negotiation::Role::RequestControl)
            .map_err(|_| GrantError::NotNegotiated)?;
        GrantBroker::from_negotiated(
            self.opened.control.clone(),
            &self.opened.transport,
            seat,
            Scope {
                parent: self.opened.binding,
                control: self.opened.routes,
                selection: &self.opened.selected,
            },
            input,
        )
    }
    /// Allocate a ticketed native configuration pair on this admitted session.
    /// The local view and unpredictable ticket are supplied by the host owner;
    /// this does not authorize a new display or create input/decoder readiness.
    pub fn offer_media_channel(
        &mut self,
        request: fr_transport::quic::ChannelRequest,
    ) -> Result<fr_transport::quic::MediaChannel, Error> {
        self.offer_media_role(request, fr_wire::attachment::MediaRole::Configuration)
    }
    /// Attach recovery/video lanes without bypassing the admitted session owner.
    pub fn offer_media_role(
        &mut self,
        request: fr_transport::quic::ChannelRequest,
        role: fr_wire::attachment::MediaRole,
    ) -> Result<fr_transport::quic::MediaChannel, Error> {
        self.check()?;
        let control = self.opened.control.clone();
        self.opened
            .transport
            .offer_media_role(
                &self.opened.cx,
                fr_transport::quic::ChannelScope {
                    control: self.opened.routes,
                    parent: self.opened.binding,
                    selection: &self.opened.selected,
                },
                request,
                role,
                || control.check().is_ok(),
            )
            .map_err(Error::Transport)
    }

    pub fn check(&mut self) -> Result<(), Error> {
        self.opened.check()
    }
    pub fn binding(&self) -> ControlBinding {
        self.opened.binding()
    }
    pub fn selection(&self) -> &Selection {
        self.opened.selection()
    }
    pub fn observation(&mut self) -> Result<ObservationControl, Error> {
        self.opened.observation()
    }
    pub fn renewed_until(&self) -> Option<HostInstant> {
        self.renewal.renewed_until()
    }
    /// A loan of this same connection for the existing media/input senders. The
    /// bound owner is checked at the next turn; never replace the loaned value.
    pub fn io(&mut self) -> Result<(&mut QuicRecords, ControlRoutes), Error> {
        self.opened.io()
    }
    pub fn close(&mut self) {
        self.opened.close();
        self.renewal.stop();
    }
    /// Run one bounded I/O turn, refreshing admission when its remaining lifetime
    /// falls below 500 ms. A required refresh may span several turns, bounded by
    /// the old proof's original deadline. QUIC, renewal and application dispatch
    /// continue throughout; no timer or unrelated traffic renews the proof.
    ///
    /// `fresh_nonce` is the qualified host-owned unpredictable nonce source, never
    /// a counter or peer value. Other records pass to the existing typed handlers.
    /// Their `Blocked` result preserves transport ownership and original expiry.
    /// Dropping even an unpolled drive closes observation and the connection.
    pub fn drive<'a>(
        &'a mut self,
        wait: Duration,
        mut fresh_nonce: impl FnMut() -> Result<u128, ()> + 'a,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()> + 'a,
    ) -> impl Future<Output = Result<(), Error>> + 'a {
        let operation = Operation {
            session: self,
            complete: false,
        };
        async move {
            let mut operation = operation;
            operation
                .session
                .drive_inner(wait, &mut fresh_nonce, &mut other)
                .await?;
            operation.complete = true;
            Ok(())
        }
    }
    async fn drive_inner(
        &mut self,
        wait: Duration,
        fresh_nonce: &mut impl FnMut() -> Result<u128, ()>,
        other: &mut impl Services,
    ) -> Result<(), Error> {
        if wait > MAX_TURN {
            return Err(Error::InvalidConfiguration);
        }
        self.check()?;
        service(
            &mut self.renewal,
            &mut self.opened.transport,
            fresh_nonce,
            other,
        )?;
        let until = self
            .opened
            .peer
            .check(&self.opened.cx, self.opened.selected.role)?;
        if until.saturating_sub(now(&self.opened.cx)?) <= REFRESH_MARGIN_US {
            pump_refresh(
                &mut self.renewal,
                &mut self.opened.transport,
                self.opened.peer.refresh(),
                RefreshTurn {
                    cx: &self.opened.cx,
                    control: &self.opened.control,
                    until,
                    wait,
                },
                fresh_nonce,
                other,
            )
            .await?;
        } else {
            self.renewal
                .drive_checked(&mut self.opened.transport, wait, || other.permitted())
                .await
                .map_err(Error::Renewal)?;
        }
        self.check()?;
        service(
            &mut self.renewal,
            &mut self.opened.transport,
            fresh_nonce,
            other,
        )?;
        self.check()
    }
}
impl Drop for HostSession {
    fn drop(&mut self) {
        self.close();
    }
}
struct Operation<'a> {
    session: &'a mut HostSession,
    complete: bool,
}
impl Drop for Operation<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.session.close();
        }
    }
}
fn service(
    renewal: &mut ObservationRenewal,
    transport: &mut QuicRecords,
    nonce: &mut impl FnMut() -> Result<u128, ()>,
    other: &mut impl Services,
) -> Result<(), Error> {
    // Application maintenance runs on both sides of observation dispatch: a
    // control response ahead of an observation response must not prevent either
    // owner from progressing, and no application work waits on LocalAPI I/O.
    other.maintain(transport, nonce)?;
    renewal
        .receive(transport, |route, bytes| other.receive(route, bytes))
        .map_err(Error::Renewal)?;
    renewal
        .service(transport, &mut *nonce)
        .map_err(Error::Renewal)?;
    other.maintain(transport, nonce)
}
#[derive(Clone, Copy)]
struct RefreshTurn<'a> {
    cx: &'a Cx,
    control: &'a ObservationControl,
    until: u64,
    wait: Duration,
}
/// Private join of the genuine `Admission::refresh` future and the same QUIC I/O.
/// A successful refresh never wins a cancellation race against an in-flight UDP
/// drive: finish that bounded turn before returning. Failure is terminal instead.
async fn pump_refresh(
    renewal: &mut ObservationRenewal,
    transport: &mut QuicRecords,
    refresh: impl Future<Output = Result<(), Error>>,
    turn: RefreshTurn<'_>,
    nonce: &mut impl FnMut() -> Result<u128, ()>,
    other: &mut impl Services,
) -> Result<(), Error> {
    let cx = turn.cx;
    let control = turn.control;
    let mut refresh = pin!(refresh);
    let mut refreshed = false;
    loop {
        let current = now(cx)?;
        if !refreshed && current >= turn.until {
            return Err(Error::Expired);
        }
        control.check().map_err(|_| Error::Authority)?;
        service(renewal, transport, nonce, other)?;
        // Never exceed the old proof's deadline while it is still pending. Once
        // refreshed, the shared live lease, not this old snapshot, authorizes I/O.
        let wait = turn
            .wait
            .min(Duration::from_micros(turn.until.saturating_sub(current)));
        {
            let mut io = pin!(renewal.drive_checked(transport, wait, || other.permitted()));
            std::future::poll_fn(|task| {
                if !refreshed {
                    let before = now(cx)?;
                    if before >= turn.until {
                        return Poll::Ready(Err(Error::Expired));
                    }
                    if let Poll::Ready(result) = refresh.as_mut().poll(task) {
                        result?;
                        // A ready LocalAPI reply can itself cross the deadline.
                        if now(cx)? >= turn.until {
                            return Poll::Ready(Err(Error::Expired));
                        }
                        refreshed = true;
                    }
                }
                control.check().map_err(|_| Error::Authority)?;
                io.as_mut().poll(task).map_err(Error::Renewal)
            })
            .await?;
            // Keep the network future alive until it has actually completed. Dropping
            // it when refresh becomes Ready would cancel a healthy QUIC connection.
        }
        if refreshed {
            return Ok(());
        }
        asupersync::runtime::yield_now().await;
    }
}

#[cfg(test)]
pub(super) mod tests;
