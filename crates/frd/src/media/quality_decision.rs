//! Bounded reports of the ORIGINAL adaptive source's operating point.
//! This is advisory state, never receiver credit, visibility or input authority.
use asupersync::cx::Cx;
use fr_core::limits::ProtocolLimits;
use fr_media::{bitrate, pacing, receiver_feedback::Admission};
use fr_transport::quic::{Messages, Priority, QuicRecords, Route};
use fr_wire::{
    WireError,
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    negotiation::{ControlBinding, Selection},
    quality_decision::{self as wire, Decision, Mode, Reason},
};

const REPORT_INTERVAL_US: u64 = 250_000;
const ADMISSION_LIFETIME_US: u64 = 250_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Binding,
    Wire(WireError),
    Admission(super::receiver_feedback::Error),
    Clock,
    SequenceExhausted,
}

/// One fixed-size snapshot of actual scheduling state. A proposed bitrate is
/// separate from the rate confirmed by a fully validated native worker reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Point {
    interval_us: u32,
    mode: Mode,
    reason: Reason,
    applied: Option<u32>,
    pending: Option<u32>,
}
impl Point {
    pub(crate) fn current(
        pacing: &pacing::Controller,
        bitrate: Option<&bitrate::Controller>,
    ) -> Option<Self> {
        let report = pacing.report()?;
        // Hold is a sampling result, not a new explanation for the current
        // cadence. Keep the last real decision from the existing bounded history.
        let reason = pacing
            .decisions()
            .last()
            .map_or(report.reason, |r| r.reason);
        Some(Self {
            interval_us: u32::try_from(report.interval_us).ok()?,
            mode: match report.mode {
                pacing::Mode::Active => Mode::Active,
                pacing::Mode::Idle => Mode::Idle,
            },
            reason: match reason {
                pacing::Reason::Hold => Reason::Hold,
                pacing::Reason::SourceWork => Reason::SourceWork,
                pacing::Reason::SendAdmission => Reason::SendAdmission,
                pacing::Reason::CaptureCredit => Reason::CaptureCredit,
                pacing::Reason::ReceiverBacklog => Reason::ReceiverBacklog,
                pacing::Reason::DecoderWork => Reason::DecoderWork,
                pacing::Reason::HeadroomProbe => Reason::HeadroomProbe,
                pacing::Reason::VerifiedIdle => Reason::VerifiedIdle,
                pacing::Reason::ChangedAfterIdle => Reason::ChangedAfterIdle,
                pacing::Reason::EvidenceGap => Reason::EvidenceGap,
            },
            applied: bitrate.map(bitrate::Controller::applied_bitrate),
            pending: bitrate
                .and_then(bitrate::Controller::pending)
                .map(bitrate::Change::bitrate),
        })
    }
    fn decision(self, sequence: u64) -> Decision {
        Decision {
            sequence,
            capture_interval_us: self.interval_us,
            mode: self.mode,
            reason: self.reason,
            applied_bitrate_bps: self.applied,
            pending_bitrate_bps: self.pending,
        }
    }
}

/// No local send queue: each attempt samples the source's current point, and
/// backpressure leaves only the previous accepted point and the rate-limit clock.
/// Already admitted records remain charged to the canonical transport budget.
pub(crate) struct Sender {
    parent: ControlBinding,
    view: Binding,
    route: Route,
    limits: ProtocolLimits,
    next: u64,
    last_now: Option<u64>,
    attempted_at: Option<u64>,
    accepted: Option<Point>,
}
impl Sender {
    pub(crate) fn selected(
        selected: &Selection,
        parent: ControlBinding,
        view: Binding,
        route: Route,
    ) -> Result<Option<Self>, Error> {
        if !selected
            .capabilities
            .iter()
            .any(|cap| cap.name == wire::CAPABILITY)
        {
            return Ok(None);
        }
        if !wire::selected(selected)
            || usize::try_from(selected.limits.max_control_message_bytes())
                .map_err(|_| Error::Binding)?
                < wire::RECORD_BYTES
            || !matches!(route, Route::Stream(stream)
                if stream.binding == parent.id
                    && stream.messages == Messages::SessionControl
                    && stream.priority == Priority::Critical
                    && stream.outbound
                    && stream.maximum >= wire::RECORD_BYTES)
        {
            return Err(Error::Binding);
        }
        let mut sender = Self {
            parent,
            view,
            route,
            limits: selected.limits,
            next: 1,
            last_now: None,
            attempted_at: None,
            accepted: None,
        };
        sender.rebind(view)?;
        Ok(Some(sender))
    }
    /// Recovery changes media scope but not the original session or its sequence
    /// floor. Clear only the old accepted snapshot; never renew a record in QUIC.
    pub(crate) fn rebind(&mut self, mut view: Binding) -> Result<(), Error> {
        view.validate().map_err(Error::Wire)?;
        if self.parent.host_boot != view.parent.host_boot
            || self.parent.os_session != view.parent.os_session
            || self.parent.remote_session != view.parent.remote_session
        {
            return Err(Error::Binding);
        }
        view.parent = self.parent;
        view.validate().map_err(Error::Wire)?;
        if view != self.view {
            self.accepted = None;
        }
        self.view = view;
        Ok(())
    }
    pub(crate) fn service(
        &mut self,
        q: &mut QuicRecords,
        cx: &Cx,
        point: Option<Point>,
        now: u64,
    ) -> Result<bool, Error> {
        let view = self.view;
        let limits = self.limits;
        let route = self.route;
        self.transmit(point, now, |decision, until| {
            let mut bytes = [0; wire::RECORD_BYTES];
            let length = wire::encode(
                decision,
                view,
                &limits,
                &mut bytes,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            )
            .map_err(Error::Wire)?;
            super::receiver_feedback::admission::send(q, cx, route, &bytes[..length], now, until)
                .map_err(Error::Admission)
        })
    }
    fn transmit(
        &mut self,
        point: Option<Point>,
        now: u64,
        mut send: impl FnMut(Decision, u64) -> Result<Admission, Error>,
    ) -> Result<bool, Error> {
        if self.last_now.is_some_and(|previous| now < previous) {
            return Err(Error::Clock);
        }
        self.last_now = Some(now);
        let Some(point) = point else { return Ok(false) };
        if self.accepted == Some(point)
            || self
                .attempted_at
                .is_some_and(|at| now - at < REPORT_INTERVAL_US)
        {
            return Ok(false);
        }
        let next = self.next.checked_add(1).ok_or(Error::SequenceExhausted)?;
        let until = now.checked_add(ADMISSION_LIFETIME_US).ok_or(Error::Clock)?;
        let decision = point.decision(self.next);
        decision.validate().map_err(Error::Wire)?;
        self.attempted_at = Some(now);
        if send(decision, until)? == Admission::Accepted {
            self.next = next;
            self.accepted = Some(point);
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests;
