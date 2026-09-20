//! Per-subscriber reference recovery on the original shared capture owner.
//! Attachment and decoder waits never borrow the encoder or revoke its consent.
use super::{Entry, Error, Subscriber, join::PendingJoin};
use crate::media_quic::replacement::Replacement;
use asupersync::cx::Cx;
use fr_transport::quic::{ControlRoutes, Disposition, QuicRecords, Route};
use fr_wire::{attachment::Ticket, negotiation::ControlBinding, recovery_request};
use std::cell::Cell;

/// Recovery describes protocol milestones only, never visible pixels or control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryState {
    Receiving,
    /// Supply exactly three fresh local tickets to `advance_recovery`.
    NeedsTickets,
    Attaching,
    DecoderStartup,
}
pub(super) struct Recovering {
    pub(super) until: u64,
    routes: ControlRoutes,
    parent: ControlBinding,
    replacement: Option<Replacement>,
}
impl Entry {
    pub(super) fn replacing(&self) -> bool {
        self.recovery.is_some() && self.join.is_none()
    }
    pub(super) fn check_media(&self, q: &QuicRecords) -> Result<(), Error> {
        if let Some(media) = &self.media {
            media.check(q).map_err(Error::Transport)
        } else if self
            .recovery
            .as_ref()
            .is_some_and(|r| r.replacement.is_some())
        {
            // Old routes are deliberately tombstoned. Original connection identity
            // and both source/viewer authorities have already been checked.
            if q.is_closed() {
                Err(Error::Closed)
            } else {
                Ok(())
            }
        } else {
            Err(Error::Closed)
        }
    }
}
impl Subscriber {
    /// Original failure deadline, shared by attachment, IDR and decoder startup.
    pub fn recovery_deadline(&mut self, q: &QuicRecords) -> Result<Option<u64>, Error> {
        self.with_entry(q, |entry| Ok(entry.recovery.as_ref().map(|r| r.until)))
    }
    pub fn recovery_state(&mut self, q: &QuicRecords) -> Result<RecoveryState, Error> {
        self.with_entry(q, |entry| {
            Ok(if entry.join.is_some() && entry.recovery.is_some() {
                RecoveryState::DecoderStartup
            } else if let Some(recovery) = &entry.recovery {
                if recovery.replacement.is_some() {
                    RecoveryState::Attaching
                } else {
                    RecoveryState::NeedsTickets
                }
            } else {
                RecoveryState::Receiving
            })
        })
    }

    /// Admit a real failure from this viewer's original negotiated control lane.
    /// Fence its old egress immediately, even while another task awaits capture.
    /// The original sender charges its failure allowance and fixes the deadline;
    /// duplicates never replenish either. Healthy viewers remain unchanged.
    pub fn request_recovery(
        &mut self,
        q: &QuicRecords,
        routes: ControlRoutes,
        parent: ControlBinding,
        route: Route,
        bytes: &[u8],
    ) -> Result<bool, Error> {
        self.with_entry(q, |entry| {
            if entry.join.is_some() || entry.starting.is_some() {
                return Err(Error::Startup(super::decoder_startup::Error::WrongState));
            }
            let media = entry.media.as_ref().ok_or(Error::Closed)?;
            media
                .check_recovery_host(q, routes, parent, &entry.sender, route)
                .map_err(Error::Transport)?;
            let Some(demand) = media
                .admit_recovery_request(q, routes, parent, &mut entry.sender, bytes)
                .map_err(Error::Transport)?
            else {
                return Ok(false);
            };
            if entry.recovery.is_some() {
                return Err(Error::Closed);
            }
            entry.recovery = Some(Recovering {
                until: demand.deadline_micros(),
                routes,
                parent,
                replacement: None,
            });
            // Do not queue an encoder request before fresh attachments are ready.
            // The retained failed cache carries the charge. After rebind the
            // original publisher coalesces this request with joins under its ONE
            // 500 ms source IDR allowance, using this same absolute deadline.
            Ok(true)
        })
    }

    /// Advance only this subscriber's three fresh role attachments. Generate
    /// tickets outside publisher locks; supply them only in `NeedsTickets` state.
    /// Every later turn uses None. Continue ordinary session renewal between
    /// bounded turns, and call `service` for the resulting decoder handshake.
    /// No new cache, source, connection, permission or failure allowance is created.
    pub fn advance_recovery(
        &mut self,
        cx: &Cx,
        q: &mut QuicRecords,
        tickets: Option<[Ticket; 3]>,
    ) -> Result<RecoveryState, Error> {
        let shared = self.members.upgrade().ok_or(Error::Closed)?;
        let mut members = shared.lock().map_err(|_| Error::Poisoned)?;
        let entry = members.entries[self.slot].as_ref().ok_or(Error::Closed)?;
        if !q.is_bound_to(&entry.connection) {
            return Err(Error::ForeignConnection);
        }
        members.tick()?;
        let owner = members.owner.clone();
        let cfg = members.configuration;
        let entry = members.entries[self.slot].as_mut().ok_or(Error::Closed)?;
        if let Some(error) = entry.failure {
            return Err(error);
        }
        let result = (|| {
            if !entry.replacing() {
                return Err(Error::InvalidBudget);
            }
            let control = entry.control.clone();
            let recovery = entry.recovery.as_mut().ok_or(Error::Closed)?;
            if recovery.replacement.is_none() {
                let tickets = tickets.ok_or(Error::InvalidBudget)?;
                let media = entry.media.take().ok_or(Error::Closed)?;
                recovery.replacement = Some(
                    media
                        .begin_replacement(
                            cx,
                            q,
                            recovery.routes,
                            recovery.parent,
                            recovery.until,
                            Some(tickets),
                            || owner.check().is_ok() && control.check().is_ok(),
                        )
                        .map_err(Error::Recovery)?,
                );
            } else if tickets.is_some() {
                return Err(Error::InvalidBudget);
            }
            let replacement = recovery.replacement.as_mut().ok_or(Error::Closed)?;
            replacement
                .advance(cx, q, || owner.check().is_ok() && control.check().is_ok())
                .map_err(Error::Recovery)?;
            if replacement.is_complete() {
                let media = recovery
                    .replacement
                    .take()
                    .ok_or(Error::Closed)?
                    .finish(cx, q)
                    .map_err(Error::Recovery)?;
                let setup = media
                    .recover_sender(q, &mut entry.sender)
                    .map_err(Error::Transport)?;
                entry.view = media.binding();
                entry.media = Some(media);
                entry.join = Some(PendingJoin::recovering(&setup, cfg, recovery.until));
            }
            Ok(if entry.join.is_some() {
                RecoveryState::DecoderStartup
            } else {
                RecoveryState::Attaching
            })
        })();
        if let Err(error) = result {
            entry.close(error);
        }
        if let Err(error) = owner.check() {
            members.close(Error::Media(error));
        }
        members.stop_if_empty();
        result
    }

    /// Leave attachment records in their original bounded transport slots so
    /// renewal dispatch cannot consume another owner's in-progress handshake.
    pub fn owns_recovery_record(&self, route: Route, bytes: &[u8]) -> bool {
        self.members
            .upgrade()
            .and_then(|m| {
                m.lock().ok().map(|members| {
                    members.entries[self.slot]
                        .as_ref()
                        .and_then(|e| e.recovery.as_ref())
                        .and_then(|r| r.replacement.as_ref())
                        .is_some_and(|r| r.owns_record(route, bytes))
                })
            })
            .unwrap_or(false)
    }

    /// Consume at most one actual recovery request before ordinary control dispatch.
    pub fn dispatch_recovery(
        &mut self,
        q: &mut QuicRecords,
        routes: ControlRoutes,
        parent: ControlBinding,
    ) -> Result<bool, Error> {
        // The state check validates original identity BEFORE consuming any bytes.
        if self.recovery_state(q)? != RecoveryState::Receiving {
            return Ok(false);
        }
        if !self.startup_complete(q)? {
            return Ok(false);
        }
        let shared = self.members.upgrade().ok_or(Error::Closed)?;
        let (owner, control) = {
            let members = shared.lock().map_err(|_| Error::Poisoned)?;
            (
                members.owner.clone(),
                members.entries[self.slot]
                    .as_ref()
                    .ok_or(Error::Closed)?
                    .control
                    .clone(),
            )
        };
        let mut bytes = [0; recovery_request::REQUEST_BYTES];
        let mut len = 0;
        let available = Cell::new(true);
        q.receive_ready(
            &control.context(),
            || owner.check().is_ok() && control.check().is_ok(),
            |route| available.get() && route == Route::Stream(routes.inbound),
            |_, record| {
                if !is_request(record) {
                    return Ok(Disposition::Blocked);
                }
                if record.len() > bytes.len() {
                    return Err(());
                }
                bytes[..record.len()].copy_from_slice(record);
                len = record.len();
                available.set(false);
                Ok(Disposition::Consumed)
            },
        )
        .map_err(|e| Error::Transport(crate::media_quic::Error::Transport(e)))?;
        if len == 0 {
            return Ok(false);
        }
        self.request_recovery(
            q,
            routes,
            parent,
            Route::Stream(routes.inbound),
            &bytes[..len],
        )
    }
}
pub(crate) fn is_request(bytes: &[u8]) -> bool {
    bytes.get(6..8) == Some(&0x0036_u16.to_be_bytes())
}
