//! A shared subscriber served by its ORIGINAL authenticated session and renewer.
//! Capture belongs to `Publisher::serve` on a sibling task, never this connection.
use super::{Error, HostSession, Services, now};
use crate::{
    media::{
        ObservationControl,
        presented::{self, HostPresentation},
        receiver_feedback::{self as feedback, HostFeedback, Setup},
        shared_publisher::Subscriber,
    },
    session_startup::Role,
};
use fr_transport::quic::{Disposition, QuicRecords, Route};
use std::{future::Future, time::Duration};

const RECORDS_PER_MAINTENANCE: usize = 16;
const SHARED_TURN: Duration = Duration::from_millis(10);

/// Stage counters, not delivery/visibility claims. No media or peer strings are
/// retained here. Failed and successful drive turns do not reset these counters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SharedStatistics {
    pub admitted_records: u64,
    pub repair_requests: u64,
    pub presentation_reports: u64,
    pub feedback_reports: u64,
    pub recovery_requests: u64,
    pub recovered_streams: u64,
}

/// Own one existing observation session plus its non-cloneable shared member.
/// All UDP, `LocalAPI` refresh, clock service and challenge renewal remain on the
/// original `HostSession`. A subscriber failure closes THIS session; it cannot
/// renew source consent, create input authority, or cancel another viewer.
pub struct SharedHost {
    session: HostSession,
    selection: fr_wire::negotiation::Selection,
    subscriber: Option<Subscriber>,
    presentation: Option<HostPresentation>,
    feedback: Option<HostFeedback>,
    repair: Route,
    statistics: SharedStatistics,
    recovery: Option<recovery::Handoff>,
}
impl HostSession {
    /// Queue a live source join using THIS session's consent and completed media
    /// attachments. No caller-supplied authority or copied connection can stand
    /// in for the original admitted owner. The queue retains the same bounded
    /// startup deadline while this session continues renewal and UDP service.
    /// Admission failure retires this moved session, not the source or its peers.
    pub fn join_shared(
        mut self,
        queue: &crate::media::shared_publisher::JoinQueue,
        media: crate::media_quic::NegotiatedMedia,
        policy: fr_media::delivery::SendPolicy,
        timeout: Duration,
    ) -> Result<SharedHost, Error> {
        self.check()?;
        if self.opened.selected.role != Role::Observe {
            return Err(Error::Order);
        }
        let subscriber = queue
            .admit(
                self.opened.control.clone(),
                media,
                &self.opened.transport,
                policy,
                timeout,
            )
            .map_err(Error::SharedPublication)?;
        self.into_shared(subscriber)
    }
    /// Bind a member admitted by the original Publisher (completed or pending)
    /// to the original observation-only session. A source/member mismatch or a
    /// control-requesting session refuses. Both moved owners are retired on
    /// failure; no authority, connection or sender is reconstructed.
    pub fn into_shared(mut self, mut subscriber: Subscriber) -> Result<SharedHost, Error> {
        self.check()?;
        if self.opened.selected.role != Role::Observe {
            return Err(Error::Order);
        }
        let view = subscriber
            .bind_session(
                &self.opened.transport,
                &self.opened.control,
                self.opened.binding,
            )
            .map_err(Error::SharedPublication)?;
        let repair = subscriber
            .repair_route(&self.opened.transport)
            .map_err(Error::SharedPublication)?;
        let presentation = HostPresentation::attach(
            &self.opened.selected,
            self.opened.binding,
            view,
            &self.opened.transport,
            self.opened.routes.inbound,
            self.opened.control.clone(),
        )
        .map_err(Error::PresentedState)?;
        let feedback = Setup::selected(&self.opened.selected, self.opened.binding, view)
            .map_err(Error::ReceiverFeedback)?
            .map(|setup| {
                HostFeedback::new(
                    setup,
                    Route::Stream(self.opened.routes.inbound),
                    Route::Stream(self.opened.routes.outbound),
                )
            })
            .transpose()
            .map_err(Error::ReceiverFeedback)?;
        Ok(SharedHost {
            selection: self.opened.selected.clone(),
            session: self,
            subscriber: Some(subscriber),
            presentation,
            feedback,
            repair,
            statistics: SharedStatistics::default(),
            recovery: None,
        })
    }
}
impl SharedHost {
    pub const fn statistics(&self) -> SharedStatistics {
        self.statistics
    }
    pub fn renewed_until(&self) -> Option<fr_core::time::HostInstant> {
        self.session.renewed_until()
    }
    /// This is the exact first-decode report, not a claim of visible presentation.
    pub fn startup_complete(&mut self) -> Result<bool, Error> {
        self.session.check()?;
        self.subscriber
            .as_mut()
            .ok_or(Error::Closed)?
            .startup_complete(&self.session.opened.transport)
            .map_err(Error::SharedPublication)
    }
    /// Revoke observation before releasing the member's retained media. Last
    /// unsubscribe is handled by the publisher; its child remains reapable there.
    pub fn close(&mut self) {
        self.session.close();
        self.subscriber = None;
    }
    /// A bounded original-session turn. Decoder replies and selective repair are
    /// serviced before general renewal dispatch, and again around native waits.
    /// Every UDP write checks source consent as well as the original peer lease.
    /// `other` handles only unrelated records; Blocked retains transport ownership.
    /// Use a qualified fresh host nonce source, never a peer value or a counter.
    /// Dropping even an UNPOLLED future retires this member and connection.
    pub fn drive<'a>(
        &'a mut self,
        wait: Duration,
        mut fresh_nonce: impl FnMut() -> Result<u128, ()> + 'a,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()> + 'a,
    ) -> impl Future<Output = Result<(), Error>> + 'a {
        let operation = SharedOperation {
            host: self,
            complete: false,
        };
        async move {
            let mut operation = operation;
            operation
                .host
                .drive_inner(wait, &mut fresh_nonce, &mut other)
                .await?;
            operation.complete = true;
            Ok(())
        }
    }
    async fn drive_inner(
        &mut self,
        wait: Duration,
        nonce: &mut impl FnMut() -> Result<u128, ()>,
        other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<(), Error> {
        self.session.check()?;
        let mut application = SharedServices {
            subscriber: self.subscriber.as_mut().ok_or(Error::Closed)?,
            control: self.session.opened.control.clone(),
            repair: &mut self.repair,
            routes: self.session.opened.routes,
            parent: self.session.opened.binding,
            selection: &self.selection,
            recovery: &mut self.recovery,
            presentation: &mut self.presentation,
            feedback: &mut self.feedback,
            statistics: &mut self.statistics,
            other,
        };
        self.session
            .drive_inner(wait, nonce, &mut application)
            .await?;
        self.session.check()
    }
    /// Run the original session continuously while the publisher captures on its
    /// sibling task. No new runtime, worker, channel or per-viewer picture FIFO.
    /// Idle sessions still refresh identity, renew observation and expire media.
    pub fn serve<'a>(
        &'a mut self,
        mut fresh_nonce: impl FnMut() -> Result<u128, ()> + 'a,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()> + 'a,
    ) -> impl Future<Output = Result<(), Error>> + 'a {
        let operation = SharedOperation {
            host: self,
            complete: false,
        };
        async move {
            let operation = operation;
            loop {
                operation
                    .host
                    .drive_inner(SHARED_TURN, &mut fresh_nonce, &mut other)
                    .await?;
                asupersync::runtime::yield_now().await;
            }
        }
    }
}
impl Drop for SharedHost {
    fn drop(&mut self) {
        self.close();
    }
}
struct SharedOperation<'a> {
    host: &'a mut SharedHost,
    complete: bool,
}
impl Drop for SharedOperation<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.host.close();
        }
    }
}
struct SharedServices<'a, F> {
    subscriber: &'a mut Subscriber,
    control: ObservationControl,
    repair: &'a mut Route,
    routes: fr_transport::quic::ControlRoutes,
    parent: fr_wire::negotiation::ControlBinding,
    selection: &'a fr_wire::negotiation::Selection,
    recovery: &'a mut Option<recovery::Handoff>,
    presentation: &'a mut Option<HostPresentation>,
    feedback: &'a mut Option<HostFeedback>,
    statistics: &'a mut SharedStatistics,
    other: &'a mut F,
}
impl<F: FnMut(Route, &[u8]) -> Result<Disposition, ()>> Services for SharedServices<'_, F> {
    fn permitted(&mut self) -> bool {
        self.control.check().is_ok() && self.subscriber.session_live().is_ok()
    }
    fn maintain<N: FnMut() -> Result<u128, ()>>(
        &mut self,
        q: &mut QuicRecords,
        nonce: &mut N,
    ) -> Result<(), Error> {
        if !self.permitted() {
            return Err(Error::Authority);
        }
        self.maintain_recovery(q, nonce)?;
        let repairs = self
            .subscriber
            .dispatch_repairs(q)
            .map_err(Error::SharedPublication)?;
        self.statistics.repair_requests = self
            .statistics
            .repair_requests
            .saturating_add(u64::try_from(repairs).map_err(|_| Error::Clock)?);
        let cx = self.control.context();
        let report = self
            .subscriber
            .service(&cx, q, RECORDS_PER_MAINTENANCE)
            .map_err(Error::SharedPublication)?;
        self.statistics.admitted_records = self
            .statistics
            .admitted_records
            .saturating_add(u64::try_from(report.accepted).map_err(|_| Error::Clock)?);
        self.finish_recovery(q)?;
        if let Some(presentation) = &mut *self.presentation {
            presentation
                .check_connection(q)
                .map_err(Error::PresentedState)?;
            // Initial and late-join members need not have a source frame yet.
            if let Some(progress) = self
                .subscriber
                .session_progress()
                .map_err(Error::SharedPublication)?
            {
                presentation
                    .observe(Some(progress))
                    .map_err(Error::PresentedState)?;
            }
        }
        // A startup peer has not installed its steady-state feedback responder.
        // Do not put a solicited query ahead of its configuration/first-decode
        // handshake or spend its timeout on unsolicited startup telemetry.
        if self
            .subscriber
            .startup_complete(q)
            .map_err(Error::SharedPublication)?
            && let Some(feedback) = &mut *self.feedback
        {
            feedback
                .service(q, &cx, now(&cx)?)
                .map_err(Error::ReceiverFeedback)?;
        }
        Ok(())
    }
    fn receive(&mut self, route: Route, bytes: &[u8]) -> Result<Disposition, ()> {
        if let Some(disposition) = self.recovery_record(route, bytes)? {
            return Ok(disposition);
        }
        if presented::is_report(bytes) {
            let presentation = self.presentation.as_mut().ok_or(())?;
            presentation
                .observe(self.subscriber.session_progress().map_err(|_| ())?)
                .map_err(|_| ())?;
            presentation.receive(route, bytes).map_err(|_| ())?;
            self.statistics.presentation_reports = presentation.accepted;
            Ok(Disposition::Consumed)
        } else if feedback::is_feedback(bytes) {
            let feedback = self.feedback.as_mut().ok_or(())?;
            feedback
                .receive(
                    route,
                    bytes,
                    self.control.check().map_err(|_| ())?.as_micros(),
                )
                .map_err(|_| ())?;
            self.statistics.feedback_reports = feedback.accepted;
            Ok(Disposition::Consumed)
        } else if route == *self.repair {
            Ok(Disposition::Blocked)
        } else {
            (self.other)(route, bytes)
        }
    }
}

#[cfg(test)]
mod tests;

mod recovery;
