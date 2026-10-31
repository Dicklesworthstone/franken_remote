//! FRD0 file envelopes -> pinned ATP frames -> actual disk-worker results.
//!
//! Settings come from the original, separately authenticated file attachment;
//! this module does not create tickets, listen on a port or grant permissions.
//! Transport ownership must retain returned reply bytes until its bounded send
//! completes and must never resubmit a file after uncertain publication.
use crate::{
    atp::receive::{self as atp, Event},
    receive::{self, DropDirectory, Publication},
    session::{self, Permission, Policy, Progress},
    worker::{self, Completion, Task},
};
use asupersync::cx::Cx;
use fr_core::input_submission::InputSession;
use fr_wire::{
    WireError,
    files::{self, Body, Context, Disposition, Limits, Message, Reason},
};

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub incoming: Context,
    pub outgoing: Context,
    pub limits: Limits,
}
impl Settings {
    fn validate(self, authority: &session::Authority) -> Result<(), Error> {
        self.incoming.validate().map_err(Error::Wire)?;
        self.outgoing.validate().map_err(Error::Wire)?;
        let scope = authority.binding();
        let direction = authority.direction();
        if self.incoming.sender != direction.source()
            || self.outgoing.sender != direction.receiver()
            || self.incoming.direction != direction
            || self.outgoing.direction != direction
        {
            return Err(Error::Wire(WireError::WrongRole));
        }
        if self.incoming.session != scope.session
            || self.outgoing.session != scope.session
            || self.incoming.lease != scope.lease
            || self.outgoing.lease != scope.lease
            || self.incoming.handle != self.outgoing.handle
            || self.incoming.channel != self.outgoing.channel
            || !authority.matches_channel(self.incoming.channel)
        {
            return Err(Error::Wire(WireError::InvalidBinding));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Wire(WireError),
    Atp(atp::Error),
    Busy,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Queued(u64),
    CancellationRequested,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResultReceipt {
    pub id: u64,
    pub outcome: Result<Completion, worker::Error>,
}

/// One pending wire reply plus one original disk slot, both explicitly bounded.
/// A reply remains owned here if serialization fails (including short buffers).
/// The last actual outcome remains inspectable even after successful encoding.
pub struct HostReceiver {
    atp: atp::Receiver,
    settings: Settings,
    policy: Policy,
    reply: Option<Event>,
    // A fresh offer rejected BEFORE disk admission is not a worker receipt.
    // Retain one content-free refusal through encoding/backpressure, separately
    // from the last actual disk outcome (which may already be published).
    offer_refusal: Option<(u64, Reason)>,
    progress: Option<Progress>,
    last: Option<ResultReceipt>,
}
/// Receive either selected direction; the legacy host name remains compatible.
pub type Receiver = HostReceiver;

impl std::fmt::Debug for HostReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostFileWire([original attachment])")
    }
}
impl HostReceiver {
    pub fn spawn(
        cx: Cx,
        input: &InputSession,
        directory: DropDirectory,
        permission: Permission,
        policy: Policy,
        settings: Settings,
    ) -> Result<(Self, Task), Error> {
        Self::spawn_with_authority(
            cx,
            session::Authority::from_input(input),
            directory,
            permission,
            policy,
            settings,
        )
    }
    pub fn spawn_with_authority(
        cx: Cx,
        authority: session::Authority,
        directory: DropDirectory,
        permission: Permission,
        policy: Policy,
        settings: Settings,
    ) -> Result<(Self, Task), Error> {
        settings.validate(&authority)?;
        let (atp, task) = atp::Receiver::spawn_with_authority(
            cx,
            authority,
            directory,
            permission,
            policy,
            settings.limits.atp_bytes(),
        )
        .map_err(Error::Atp)?;
        Ok((
            Self {
                atp,
                settings,
                policy,
                reply: None,
                offer_refusal: None,
                progress: None,
                last: None,
            },
            task,
        ))
    }
    pub fn stop(&self) {
        self.atp.stop();
    }
    pub fn is_closed(&self) -> bool {
        self.atp.is_closed()
    }
    pub fn is_busy(&self) -> bool {
        self.reply.is_some() || self.offer_refusal.is_some() || self.atp.has_pending()
    }
    pub(crate) fn has_pending_offer_refusal(&self) -> bool {
        self.offer_refusal.is_some()
    }
    pub fn progress(&self) -> Option<Progress> {
        if self.is_closed() {
            None
        } else {
            self.progress
        }
    }
    pub fn last_result(&self) -> Option<ResultReceipt> {
        self.last
    }

    /// Parse only with the installed role, channel, lease and directory binding.
    /// Cancel bypasses backpressure; no other record is enqueued before the prior
    /// actual receipt has been polled and its bounded reply encoded. A terminal
    /// pre-admission refusal is retained even if a later cancellation arrives.
    pub fn receive(&mut self, bytes: &[u8]) -> Result<Admission, Error> {
        let message = files::decode(bytes, self.settings.incoming, self.settings.limits)
            .map_err(Error::Wire)?;
        if self.has_pending_offer_refusal() {
            return Err(Error::Busy);
        }
        if let Body::Cancel(_) = message.body {
            self.atp
                .cancel(self.atp.binding(), message.id)
                .map_err(Error::Atp)?;
            return Ok(Admission::CancellationRequested);
        }
        if self.is_busy() {
            return Err(Error::Busy);
        }
        let (atp, offer, profile) = match message.body {
            Body::Offer { atp, profile } => (atp, true, profile),
            Body::Chunk { atp } => (atp, false, self.atp.profile()),
            _ => return Err(Error::Wire(WireError::WrongRole)),
        };
        // Only a NEW offer with no active or pending disk work can certify
        // pre-admission refusal. In particular, an old transfer ID or a rejected
        // completion must never overwrite publication evidence with "no effect".
        let fresh_offer = offer
            && !self.atp.is_closed()
            && self.progress.is_none()
            && self.last.is_none_or(|last| message.id > last.id);
        let result =
            self.atp
                .push_enveloped(self.atp.binding(), message.id, atp, Some(offer), profile);
        if let Err(error) = result
            && fresh_offer
            && let Some(reason) = offer_refusal(error)
        {
            self.atp.stop();
            self.offer_refusal = Some((message.id, reason));
        }
        result.map(Admission::Queued).map_err(Error::Atp)
    }

    /// `FileAccept` is emitted only after real staging admission. `FileComplete`
    /// carries an actual worker outcome or a fresh offer's pre-admission refusal;
    /// the latter never manufactures a disk receipt. Staging is not publication.
    /// No network receipt, first byte or queued completion grants input authority.
    pub fn poll_reply(&mut self, output: &mut [u8]) -> Result<Option<usize>, Error> {
        if let Some((id, reason)) = self.offer_refusal {
            let size = files::encode(
                Message {
                    id,
                    body: Body::Complete {
                        disposition: Disposition::Refused,
                        reason,
                        published_bytes: 0,
                        atp: &[],
                    },
                },
                self.settings.outgoing,
                self.settings.limits,
                output,
            )
            .map_err(Error::Wire)?;
            self.offer_refusal = None;
            return Ok(Some(size));
        }
        if self.reply.is_none() {
            let Some(event) = self.atp.poll().map_err(Error::Atp)? else {
                return Ok(None);
            };
            self.last = Some(ResultReceipt {
                id: event.id,
                outcome: event.outcome(),
            });
            match event.outcome() {
                Ok(Completion::Begun(progress)) => self.progress = Some(progress),
                Ok(Completion::Written(progress)) => {
                    self.progress = Some(progress);
                    return Ok(None);
                }
                _ => self.progress = None,
            }
            self.reply = Some(event);
        }
        let event = self.reply.as_ref().expect("reply retained");
        let mut buffer = [0_u8; atp::MAX_REPLY_BYTES];
        let closed_begin =
            matches!(event.outcome(), Ok(Completion::Begun(_))) && self.atp.is_closed();
        let n = if closed_begin {
            0
        } else {
            event
                .encode_reply(&mut buffer)
                .map_err(Error::Atp)?
                .unwrap_or(0)
        };
        let body = self.reply_body(event, &buffer[..n], closed_begin)?;
        let size = files::encode(
            Message { id: event.id, body },
            self.settings.outgoing,
            self.settings.limits,
            output,
        )
        .map_err(Error::Wire)?;
        // Clear only after the exact actual result has been encoded successfully.
        // The transport, not this method, still owns proof-delivery uncertainty.
        self.reply = None;
        Ok(Some(size))
    }
    fn reply_body<'a>(
        &self,
        event: &Event,
        atp: &'a [u8],
        closed_begin: bool,
    ) -> Result<Body<'a>, Error> {
        if closed_begin {
            return Ok(Body::Complete {
                disposition: Disposition::Refused,
                reason: Reason::Cancelled,
                published_bytes: 0,
                atp: &[],
            });
        }
        Ok(match event.outcome() {
            Ok(Completion::Begun(progress)) => Body::Accept {
                profile: event.profile(),
                size: progress.total_bytes,
                bytes_per_second: self.policy.bytes_per_second,
                chunk_bytes: u32::try_from(
                    self.settings
                        .limits
                        .atp_bytes()
                        .saturating_sub(files::ATP_DATA_OVERHEAD),
                )
                .map_err(|_| Error::Wire(WireError::InvalidLimits))?,
                concurrent_transfers: 1,
                atp,
            },
            Ok(Completion::Published(receipt)) => Body::Complete {
                disposition: match receipt.publication {
                    Publication::Durable => Disposition::PublishedDurable,
                    Publication::DurabilityUnknown => Disposition::PublishedDurabilityUnknown,
                },
                reason: Reason::None,
                published_bytes: receipt.bytes,
                atp,
            },
            Err(worker::Error::UnknownEffect) => Body::Complete {
                disposition: Disposition::UnknownEffect,
                reason: Reason::UnknownEffect,
                published_bytes: 0,
                atp: &[],
            },
            Err(error) => Body::Complete {
                disposition: Disposition::Refused,
                reason: refusal(error),
                published_bytes: 0,
                atp: &[],
            },
            Ok(Completion::Cancelled) => Body::Complete {
                disposition: Disposition::Refused,
                reason: Reason::Cancelled,
                published_bytes: 0,
                atp: &[],
            },
            Ok(Completion::Written(_)) => return Err(Error::Atp(atp::Error::Order)),
        })
    }
}
fn refusal(error: worker::Error) -> Reason {
    match error {
        worker::Error::Closed => Reason::Cancelled,
        worker::Error::Session(session::Error::Expired | session::Error::Closed) => Reason::Expired,
        worker::Error::Session(session::Error::Permission | session::Error::Authority(_)) => {
            Reason::Permission
        }
        worker::Error::Session(session::Error::Storage(receive::Error::Integrity)) => {
            Reason::Integrity
        }
        worker::Error::Session(session::Error::Storage(receive::Error::Conflict)) => {
            Reason::Conflict
        }
        worker::Error::Session(
            session::Error::Quota
            | session::Error::RateLimited
            | session::Error::Storage(receive::Error::Quota),
        )
        | worker::Error::Allocation => Reason::Resource,
        worker::Error::UnknownEffect => Reason::UnknownEffect,
        _ => Reason::Invalid,
    }
}

// Only failures known to precede disk admission can become a no-effect reply.
// Busy remains backpressure; worker failure/poison/unknown effect cannot be
// relabelled refusal merely because no publication receipt has arrived yet.
fn offer_refusal(error: atp::Error) -> Option<Reason> {
    match error {
        atp::Error::Limits
        | atp::Error::ResourceLimit
        | atp::Error::Worker(worker::Error::Allocation | worker::Error::SequenceExhausted) => {
            Some(Reason::Resource)
        }
        atp::Error::WrongBinding
        | atp::Error::WrongTransfer
        | atp::Error::Frame
        | atp::Error::Manifest
        | atp::Error::UnsupportedProfile
        | atp::Error::Order
        | atp::Error::BufferTooSmall
        | atp::Error::Worker(
            worker::Error::InvalidName | worker::Error::InvalidChunk | worker::Error::WrongBinding,
        ) => Some(Reason::Invalid),
        atp::Error::Busy | atp::Error::Closed | atp::Error::Worker(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_admission_refusals_preserve_invalid_and_resource_categories() {
        for error in [
            atp::Error::Frame,
            atp::Error::Manifest,
            atp::Error::UnsupportedProfile,
            atp::Error::Order,
            atp::Error::Worker(worker::Error::InvalidName),
        ] {
            assert_eq!(offer_refusal(error), Some(Reason::Invalid));
        }
        for error in [
            atp::Error::Limits,
            atp::Error::ResourceLimit,
            atp::Error::Worker(worker::Error::Allocation),
            atp::Error::Worker(worker::Error::SequenceExhausted),
        ] {
            assert_eq!(offer_refusal(error), Some(Reason::Resource));
        }
    }

    #[test]
    fn backpressure_and_uncertain_worker_effects_never_become_refusal() {
        for error in [
            atp::Error::Busy,
            atp::Error::Closed,
            atp::Error::Worker(worker::Error::Busy),
            atp::Error::Worker(worker::Error::UnknownEffect),
            atp::Error::Worker(worker::Error::Session(session::Error::UnknownEffect)),
            atp::Error::Worker(worker::Error::Poisoned),
            atp::Error::Worker(worker::Error::Panicked),
            atp::Error::Worker(worker::Error::Cancelled),
        ] {
            assert_eq!(offer_refusal(error), None);
        }
    }
}
