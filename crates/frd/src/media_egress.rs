//! One prepared media record retained until the transport admits it.
//!
//! Packetization consumes a sender cursor. A full QUIC queue must NOT cause the
//! caller to ask for another packet, reconstruct a header, or extend its deadline.
//! This owner joins that cursor to transport backpressure without owning the
//! shared capture worker, input lease, or transport's connection lifetime.
use crate::media::{CaptureUpdate, Error, Subscription};
use fr_media::{access_unit::EncodedAccessUnit, delivery::PacketOffer};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Original,
    Repair,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Accepted,
    Backpressure,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Idle,
    Pending(PacketOffer),
    Accepted(PacketOffer),
}
pub enum EgressError<E> {
    Media(Error),
    Transport(E),
    Allocation,
    Closed,
}
impl<E> fmt::Debug for EgressError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Media(error) => f.debug_tuple("Media").field(error).finish(),
            // A native/foreign transport error may include peer text or secrets.
            Self::Transport(_) => f.write_str("Transport"),
            Self::Allocation => f.write_str("Allocation"),
            Self::Closed => f.write_str("Closed"),
        }
    }
}
/// The buffer remains charged while backpressured. There is no packet FIFO and
/// no access to the underlying sender cursor while a record is pending.
/// Drop/close abandons only this subscription, never another viewer's encoder.
pub struct Egress {
    subscription: Option<Subscription>,
    buffer: Vec<u8>,
    pending: Option<PacketOffer>,
    maximum: usize,
}
impl fmt::Debug for Egress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Egress")
            .field("maximum", &self.maximum)
            .field("allocated_bytes", &self.buffer.capacity())
            .field("pending", &self.pending)
            .field("closed", &self.subscription.is_none())
            .finish_non_exhaustive()
    }
}
impl Egress {
    pub fn new(subscription: Subscription) -> Self {
        let maximum = subscription.record_bytes();
        Self {
            subscription: Some(subscription),
            buffer: Vec::new(),
            pending: None,
            maximum,
        }
    }
    pub(crate) fn shared_capture_credit(
        &mut self,
        source: &crate::media::CaptureSource,
        charged: usize,
    ) -> Result<bool, Error> {
        let ready = self
            .subscription
            .as_mut()
            .ok_or(Error::Send(fr_media::delivery::SendError::Closed))?
            .shared_capture_credit(source, charged)?;
        Ok(ready && self.pending.is_none())
    }
    pub(crate) fn stream_subscription(&self) -> Result<&Subscription, Error> {
        self.subscription
            .as_ref()
            .ok_or(Error::Send(fr_media::delivery::SendError::Closed))
    }
    /// Fence this subscription on a validated failure without destroying its
    /// generation-persistent recovery allowance. Invalid records leave any
    /// prepared healthy packet intact; accepted failures retire it immediately.
    pub fn request_recovery(
        &mut self,
        source: &mut crate::media::CaptureSource,
        bytes: &[u8],
        binding: fr_wire::decoder::Binding,
    ) -> Result<bool, Error> {
        let accepted = self
            .subscription
            .as_mut()
            .ok_or(Error::Send(fr_media::delivery::SendError::Closed))?
            .request_recovery(source, bytes, binding)?;
        self.pending = None;
        self.buffer.fill(0);
        Ok(accepted)
    }
    /// Admit on the network owner while the capture worker is still borrowed.
    /// The original subscription, not a new sender, owns the unique demand.
    pub(crate) fn admit_recovery_request(
        &mut self,
        bytes: &[u8],
        binding: fr_wire::decoder::Binding,
    ) -> Result<Option<fr_media::delivery::RecoveryDemand>, Error> {
        let demand = self
            .subscription
            .as_mut()
            .ok_or(Error::Send(fr_media::delivery::SendError::Closed))?
            .admit_recovery_request(bytes, binding)?;
        self.pending = None;
        self.buffer.fill(0);
        Ok(demand)
    }
    /// Rebind the ORIGINAL failed subscription after fresh channel admission.
    /// Never construct a new cache here: that would refill recovery/repair
    /// credit. The session must fence/retire the old transport lanes first.
    pub fn recover(
        &mut self,
        epoch: fr_media::delivery::MediaEpoch,
        bindings: fr_media::delivery::MediaBindings,
    ) -> Result<(), Error> {
        self.subscription
            .as_mut()
            .ok_or(Error::Send(fr_media::delivery::SendError::Closed))?
            .recover(epoch, bindings)?;
        self.pending = None;
        self.buffer.fill(0);
        Ok(())
    }
    pub fn enqueue(&mut self, unit: EncodedAccessUnit) -> Result<(), Error> {
        self.subscription
            .as_mut()
            .ok_or(Error::Send(fr_media::delivery::SendError::Closed))?
            .enqueue(unit)
    }
    /// Native source provenance and each viewer's logical budget remain checked.
    /// Shared payloads do not bypass the final per-packet authority/pacing gate.
    pub fn enqueue_shared_capture(
        &mut self,
        update: &crate::media::SharedCaptureUpdate,
    ) -> Result<(), Error> {
        self.subscription
            .as_mut()
            .ok_or(Error::Send(fr_media::delivery::SendError::Closed))?
            .enqueue_shared_capture(update)
    }
    pub fn enqueue_capture(&mut self, update: CaptureUpdate) -> Result<(), Error> {
        self.subscription
            .as_mut()
            .ok_or(Error::Send(fr_media::delivery::SendError::Closed))?
            .enqueue_capture(update)
    }
    pub fn queue_repair(&mut self, request: &[u8]) -> Result<(), Error> {
        self.subscription
            .as_mut()
            .ok_or(Error::Send(fr_media::delivery::SendError::Closed))?
            .queue_repair(request)
    }
    /// Service even without capture or network traffic. Expiry retires pending
    /// work; no later send or repair may extend the original cache lifetime.
    pub fn tick(&mut self) -> Result<(), Error> {
        let subscription = self
            .subscription
            .as_mut()
            .ok_or(Error::Send(fr_media::delivery::SendError::Closed))?;
        let result = subscription.tick().and_then(|()| {
            if let Some(offer) = self.pending.as_ref() {
                subscription.authorize_write(offer)?;
            }
            Ok(())
        });
        if result.is_err() {
            self.close();
        }
        result
    }
    pub fn next_deadline(&self) -> Option<fr_core::time::HostInstant> {
        self.subscription
            .as_ref()
            .and_then(Subscription::next_deadline)
            .into_iter()
            .chain(
                self.pending
                    .as_ref()
                    .map(|offer| fr_core::time::HostInstant::from_micros(offer.send_by_micros())),
            )
            .min()
    }
    pub fn cache_usage(&self) -> fr_media::delivery::BudgetUsage {
        self.subscription.as_ref().map_or(
            fr_media::delivery::BudgetUsage {
                bytes: 0,
                pictures: 0,
            },
            Subscription::cache_usage,
        )
    }
    pub const fn pending(&self) -> Option<&PacketOffer> {
        self.pending.as_ref()
    }
    pub fn allocated_bytes(&self) -> usize {
        self.buffer.capacity()
    }
    pub fn is_closed(&self) -> bool {
        self.subscription.is_none()
    }
    pub fn close(&mut self) {
        if let Some(subscription) = &mut self.subscription {
            subscription.fence_shared_view();
        }
        self.subscription = None;
        self.buffer = Vec::new();
        self.pending = None;
    }
    /// `send` must invoke the supplied guard immediately before native enqueue,
    /// including after any allocation/preparation that could stall. Accepted is
    /// transport admission, NOT delivery/decode/presentation. A transport error
    /// makes this sender terminal: a partial/uncertain enqueue is never replayed.
    /// A pending record retains precedence even if the caller changes `lane`.
    pub fn transmit<E>(
        &mut self,
        lane: Lane,
        send: impl FnOnce(
            &PacketOffer,
            &[u8],
            &mut dyn FnMut() -> Result<(), Error>,
        ) -> Result<Admission, E>,
    ) -> Result<Progress, EgressError<E>> {
        let Some(subscription) = self.subscription.as_mut() else {
            return Err(EgressError::Closed);
        };
        if self.buffer.is_empty() {
            if self.buffer.try_reserve_exact(self.maximum).is_err()
                || self.buffer.capacity() > self.maximum
            {
                self.close();
                return Err(EgressError::Allocation);
            }
            self.buffer.resize(self.maximum, 0);
        }
        if self.pending.is_none() {
            let result = match lane {
                Lane::Original => subscription.next_packet(&mut self.buffer),
                Lane::Repair => subscription.next_repair(&mut self.buffer),
            };
            match result {
                Ok(None) => return Ok(Progress::Idle),
                Ok(Some(offer)) => self.pending = Some(offer),
                Err(error) => {
                    self.close();
                    return Err(EgressError::Media(error));
                }
            }
        }
        let offer = self.pending.as_ref().expect("prepared above");
        if let Err(error) = subscription.authorize_write(offer) {
            self.close();
            return Err(EgressError::Media(error));
        }
        let mut guard = || subscription.authorize_write(offer);
        match send(offer, &self.buffer[..offer.byte_len()], &mut guard) {
            Ok(Admission::Backpressure) => Ok(Progress::Pending(offer.clone())),
            Ok(Admission::Accepted) => Ok(Progress::Accepted(
                self.pending.take().expect("prepared above"),
            )),
            Err(error) => {
                self.close();
                Err(EgressError::Transport(error))
            }
        }
    }
}
