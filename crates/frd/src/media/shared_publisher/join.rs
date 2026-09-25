//! Bounded late joins on the actual continuing source and original connections.
use super::{
    BOOTSTRAP_US, Entry, Error, MAX_SUBSCRIBERS, MediaError, Members, NegotiatedMedia,
    ObservationControl, Publisher, SendReport, SharedCaptureUpdate, Subscriber, Subscription,
    decoder_startup, same_source_view, same_task,
};
use crate::media::PreparedSharedCapture;
use fr_media::{delivery::SendPolicy, worker::Configuration};
use fr_transport::quic::QuicRecords;
use std::{
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

/// Weak access to ONE running source's bounded join slots. This contains no
/// worker, transport or observation grant and cannot prolong source lifetime.
/// Queue only after original session consent and media attachments have completed.
#[derive(Clone)]
pub struct JoinQueue {
    pub(super) members: Weak<Mutex<Members>>,
}
impl std::fmt::Debug for JoinQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedJoinQueue([original source])")
    }
}
impl Publisher {
    /// May be used by connection tasks while `serve` exclusively owns capture.
    pub fn join_queue(&self) -> JoinQueue {
        JoinQueue {
            members: Arc::downgrade(&self.members),
        }
    }
}
impl JoinQueue {
    /// Original local selected-display metadata only. Callers must obtain the
    /// viewer's observation consent before disclosing it. This snapshot grants
    /// neither consent nor freshness and never exposes neighboring displays.
    pub fn selected_catalog(&self) -> Result<fr_wire::display::Catalog, Error> {
        let shared = self.members.upgrade().ok_or(Error::Closed)?;
        let mut members = shared.lock().map_err(|_| Error::Poisoned)?;
        members.tick()?;
        members.selected_catalog.ok_or(Error::WrongSource)
    }
    pub(crate) fn check_source(&self) -> Result<(), Error> {
        let shared = self.members.upgrade().ok_or(Error::Closed)?;
        shared.lock().map_err(|_| Error::Poisoned)?.tick()
    }
    /// Reserve one of the same eight subscriber slots. Await a fresh rate-admitted
    /// source IDR, then drive `Subscriber::service` on this original connection.
    /// No media is sent until `DecoderConfigured`; readiness needs `FirstDecoded`.
    /// The call-time deadline covers WAITING, configuration and first decode.
    /// Full/foreign/duplicate admission refuses before taking any source credit.
    pub fn admit(
        &self,
        control: ObservationControl,
        media: NegotiatedMedia,
        transport: &QuicRecords,
        policy: SendPolicy,
        timeout: Duration,
    ) -> Result<Subscriber, Error> {
        let us = u64::try_from(timeout.as_micros()).map_err(|_| Error::InvalidBudget)?;
        if us == 0 || us > BOOTSTRAP_US {
            return Err(Error::InvalidBudget);
        }
        let shared = self.members.upgrade().ok_or(Error::Closed)?;
        let mut members = shared.lock().map_err(|_| Error::Poisoned)?;
        let view = media.binding();
        // Complete opaque attachment proofs and the original observation-only
        // role are required; numeric routes or a peer's proposed view are not.
        media
            .check_shared_publication(transport, view)
            .map_err(Error::Transport)?;
        if !members.selected_view(view)
            || control.same_owner(&members.owner)
            || members.owner.belongs_to_session(view.parent.remote_session)
            || same_task(&control, &members.owner)
            || members.anchor.is_none_or(|a| !same_source_view(a, view))
            || members.entries.iter().flatten().any(|e| {
                e.control.same_owner(&control)
                    || same_task(&e.control, &control)
                    || e.view.parent.remote_session == view.parent.remote_session
            })
        {
            return Err(Error::WrongSource);
        }
        let slot = members
            .entries
            .iter()
            .position(Option::is_none)
            .ok_or(Error::Full)?;
        members.tick()?;
        if !members.entries.iter().flatten().any(|entry| {
            entry.failure.is_none()
                && entry.join.is_none()
                && entry.starting.is_none()
                && entry.recovery.is_none()
        }) {
            return Err(Error::NoSubscribers);
        }
        control.check().map_err(Error::Media)?;
        let until = members.last.checked_add(us).ok_or(Error::JoinExpired)?;
        let setup = media
            .decoder_setup(transport, timeout)
            .map_err(Error::Startup)?
            .capped_at(until);
        let sender = media
            .sender(transport, control.clone(), policy)
            .map_err(Error::Transport)?;
        let cfg = members.configuration;
        members.entries[slot] = Some(Entry {
            connection: transport.binding(),
            media: Some(media),
            view,
            recovery: None,
            control,
            sender,
            failure: None,
            starting: None,
            cursor: super::cursor::EntryCursor::default(),
            audio: super::audio::EntryAudio::default(),
            join: Some(PendingJoin {
                setup,
                cfg,
                until,
                bootstrap: None,
                host: None,
                configuration_sent: false,
                configured: false,
            }),
        });
        Ok(Subscriber {
            members: Arc::downgrade(&shared),
            slot,
        })
    }
}

pub(super) struct PendingJoin {
    setup: decoder_startup::Setup,
    cfg: Configuration,
    until: u64,
    bootstrap: Option<SharedCaptureUpdate>,
    host: Option<decoder_startup::Host>,
    configuration_sent: bool,
    configured: bool,
}
impl PendingJoin {
    pub(super) fn recovering(
        setup: &decoder_startup::Setup,
        cfg: Configuration,
        until: u64,
    ) -> Self {
        Self {
            setup: *setup,
            cfg,
            until,
            bootstrap: None,
            host: None,
            configuration_sent: false,
            configured: false,
        }
    }
    fn waiting(&self) -> bool {
        self.bootstrap.is_none() && self.host.is_none()
    }
}
impl Entry {
    fn waiting(&self) -> bool {
        self.join.as_ref().is_some_and(PendingJoin::waiting)
    }
    pub(super) fn tick(&mut self, now: u64) -> Result<(), Error> {
        self.control.check().map_err(Error::Media)?;
        if let Some(recovery) = &self.recovery {
            if now >= recovery.until {
                return Err(Error::RecoveryExpired);
            }
            if self.replacing() {
                return Ok(());
            }
        }
        if let Some(starting) = &mut self.starting {
            starting.host.tick().map_err(Error::Startup)?;
        }
        if let Some(join) = &mut self.join {
            if now >= join.until {
                return Err(Error::JoinExpired);
            }
            if let Some(host) = &mut join.host {
                host.tick().map_err(Error::Startup)?;
            }
        }
        self.sender.tick().map_err(Error::Transport)
    }
    pub(super) fn deadline(&self) -> Option<u64> {
        if self.replacing() {
            return self.recovery.as_ref().map(|r| r.until);
        }
        self.sender
            .next_deadline()
            .map(fr_core::time::HostInstant::as_micros)
            .into_iter()
            .chain(self.join.as_ref().map(|j| j.until))
            .chain(self.recovery.as_ref().map(|r| r.until))
            .chain(self.starting.as_ref().map(|s| s.host.deadline_us()))
            .min()
    }
    pub(super) fn publish(&mut self, update: &SharedCaptureUpdate) -> Result<bool, Error> {
        // An already-issued source capture may complete after this viewer failed.
        // Healthy subscribers still consume it; a failed sender never does.
        if self.replacing() {
            return Ok(false);
        }
        if self.waiting() {
            if !update.encoded().is_some_and(|frame| frame.kind().is_idr()) {
                // No reference chain exists yet. Never bootstrap with a P picture
                // or certify the old bootstrap via a later unchanged observation.
                return Ok(false);
            }
            self.sender
                .enqueue_shared_capture(update)
                .map_err(Error::Transport)?;
            self.join.as_mut().expect("waiting").bootstrap = Some(update.clone());
        } else {
            self.sender
                .enqueue_shared_capture(update)
                .map_err(Error::Transport)?;
        }
        Ok(true)
    }
    /// Configure on the original peer; the retained alias is consumed once while
    /// the SAME sender retains the first IDR and its subsequent reference chain.
    pub(super) fn service_join(
        &mut self,
        transport: &mut QuicRecords,
        owner: &ObservationControl,
        report: &mut SendReport,
    ) -> Result<bool, Error> {
        let Some(join) = &mut self.join else {
            return Ok(true);
        };
        if join.waiting() {
            return Ok(false);
        }
        if join.host.is_none() {
            join.host = Some(
                decoder_startup::Host::new_shared(
                    self.control.clone(),
                    transport,
                    join.setup,
                    join.cfg,
                    join.bootstrap.take().expect("assigned source bootstrap"),
                )
                .map_err(Error::Startup)?,
            );
        }
        let host = join.host.as_mut().expect("retained handshake");
        if !join.configuration_sent {
            join.configuration_sent = host
                .transmit_authorized(transport, || owner.check().is_ok())
                .map_err(Error::Startup)?;
            if !join.configuration_sent {
                return Ok(false);
            }
            report.accepted += 1;
        }
        host.dispatch(transport).map_err(Error::Startup)?;
        if !join.configured
            && let Some(bootstrap) = host.take_shared_recovery().map_err(Error::Startup)?
        {
            // Already charged to this sender on its reliable recovery lane.
            // Re-enqueueing here would duplicate or restart its original budget.
            drop(bootstrap);
            join.configured = true;
        }
        if host.is_complete() {
            let (control, view) = join
                .host
                .take()
                .expect("completed")
                .finish_stream(transport)
                .map_err(Error::Startup)?;
            if !control.same_owner(&self.control) || view != self.view {
                return Err(Error::WrongSource);
            }
            self.join = None;
            self.recovery = None;
            return Ok(true);
        }
        Ok(join.configured)
    }
}
impl Members {
    /// Provisional senders occupy bounded slots, never count as healthy capture
    /// demand, and cannot bypass physical or per-subscriber logical byte credit.
    pub(super) fn capture_credit(
        &mut self,
        prepared: &PreparedSharedCapture<'_>,
    ) -> Result<(usize, Option<u64>), Error> {
        self.tick()?;
        let mut refused = 0;
        let mut ready = [false; MAX_SUBSCRIBERS];
        let mut healthy_credit = false;
        let mut join_until: Option<u64> = None;
        for (entry, ready) in self.entries.iter_mut().zip(&mut ready) {
            if let Some(entry) = entry.as_mut().filter(|e| e.failure.is_none()) {
                // Preserve the initial-pending cohort's original unconfigured
                // backpressure and single next-reference retention policy.
                if entry.replacing() || entry.starting.as_ref().is_some_and(|s| !s.seeded) {
                    continue;
                }
                let credit = if entry.join.is_some() {
                    entry.sender.shared_join_credit(prepared)
                } else {
                    entry.sender.shared_publisher_credit(prepared)
                };
                match credit {
                    Ok(credit) => {
                        *ready = credit;
                        healthy_credit |=
                            credit && (entry.join.is_none() || entry.recovery.is_some());
                        if credit && entry.waiting() {
                            let until = entry.join.as_ref().expect("waiting").until;
                            join_until = Some(join_until.map_or(until, |old| old.min(until)));
                        }
                    }
                    Err(error) => {
                        entry.close(Error::Transport(error));
                        refused += 1;
                    }
                }
            }
        }
        self.stop_if_empty();
        if self.closed {
            return Err(Error::Closed);
        }
        if !healthy_credit {
            return Err(Error::Media(MediaError::Backpressure));
        }
        for (entry, ready) in self.entries.iter_mut().zip(ready) {
            if let Some(entry) = entry.as_mut().filter(|e| e.failure.is_none())
                && !entry.replacing()
                && !ready
            {
                entry.close(Error::SlowSubscriber);
                refused += 1;
            }
        }
        Ok((refused, join_until))
    }
}
impl Subscriber {
    /// Readiness is the matching first-decode report, never physical visibility
    /// or an input grant. Service the join before querying this milestone.
    pub fn is_ready(&mut self, transport: &QuicRecords) -> Result<bool, Error> {
        self.with_entry(transport, |entry| {
            Ok(entry.join.is_none() && entry.starting.is_none() && entry.recovery.is_none())
        })
    }
}
impl Subscription {
    pub(crate) fn shared_join_credit(
        &self,
        source: &super::CaptureSource,
        charged: usize,
    ) -> Result<bool, MediaError> {
        self.control.check()?;
        if self.epoch.configuration != source.configuration.generation
            || self.first != self.capture_source.is_none()
            || self
                .capture_source
                .as_ref()
                .is_some_and(|id| !Arc::ptr_eq(id, &source.source))
        {
            return Err(MediaError::InvalidFrame);
        }
        // A slow decoder may retain its bootstrap and a short dependent chain,
        // not stall the source or build a GOP replay queue. This extra ceiling
        // never raises the original sender's negotiated byte/count/time limits.
        Ok(self.cache.cached_pictures() < 4 && self.cache.can_push_capacity(charged))
    }
}

impl Subscriber {
    /// Used only during the synchronous handoff from the original display
    /// bootstrap. Clamp waiting AND decoder setup to that call-time budget;
    /// elapsed time between stages must never create a later deadline.
    pub(crate) fn cap_join_deadline(&mut self, q: &QuicRecords, until: u64) -> Result<(), Error> {
        self.with_entry(q, |entry| {
            if let Some(join) = &mut entry.join {
                if join.host.is_some() {
                    return Err(Error::WrongSource);
                }
                join.until = join.until.min(until);
                join.setup = join.setup.capped_at(join.until);
            }
            Ok(())
        })
    }
}
