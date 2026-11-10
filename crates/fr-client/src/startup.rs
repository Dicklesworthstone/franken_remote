//! Native v0 startup on the client's existing authenticated control streams.
//! This owns protocol ordering and ONE pending record, not identity, transport,
//! decoder readiness, or input authority. A transport adapter calls `sent` only
//! after admission, and `bound` only after installing the exact control pair.
use fr_core::ids::RemoteSessionId;
use fr_wire::{
    Kind,
    directory::{self, Assembler, Listing},
    inspection,
    negotiation::{self, ControlBinding, Message, Offer, Selection},
};
use std::fmt;

const MAX_TIMEOUT_US: u64 = 60_000_000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Protocol(negotiation::Error),
    Order,
    Clock,
    Expired,
    /// This client's startup deadline ended while the host was still asking
    /// its local user to approve the connection (`ApprovalRequired`).
    ApprovalUnanswered,
    Closed,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
impl From<negotiation::Error> for Error {
    fn from(e: negotiation::Error) -> Self {
        Self::Protocol(e)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Hello,
    Capabilities,
    Selection,
    Open,
    Bind,
    Ack,
    Complete,
    InspectionClose,
    InspectionClosed,
    Inspected,
    DirectoryRequest,
    DirectoryListing,
    Listed,
    Closed,
}
/// Metadata only. Neither this value nor a successful `BindingAccepted` is a
/// usable view, input grant, or proof of visible presentation.
#[derive(Clone, PartialEq, Eq)]
pub struct Opened {
    pub binding: ControlBinding,
    pub selection: Selection,
    /// Host-clock timestamp. Do NOT compare with the client's monotonic clock.
    pub observation_until_us: u64,
}
impl fmt::Debug for Opened {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Opened([negotiated metadata, not input authority])")
    }
}
/// A local notification, not an approval RPC or a client-clock deadline.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ApprovalNotice {
    pub request: RemoteSessionId,
    pub host_deadline_us: u64,
}
impl fmt::Debug for ApprovalNotice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApprovalNotice([redacted])")
    }
}
pub struct Startup {
    offer: Offer,
    selection: Option<Selection>,
    opened: Option<Opened>,
    approval: Option<ApprovalNotice>,
    inspection: bool,
    inspected: Option<Offer>,
    directory: bool,
    assembler: Option<Assembler>,
    listed: Option<Listing>,
    created_us: u64,
    phase: Phase,
    bytes: [u8; negotiation::MAX_RECORD],
    len: usize,
    maximum: usize,
    last_us: u64,
    until_us: u64,
}
impl fmt::Debug for Startup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Startup")
            .field("phase", &self.phase)
            .field("pending_bytes", &self.len)
            .finish_non_exhaustive()
    }
}
impl Startup {
    pub fn new(offer: Offer, now_us: u64, timeout_us: u64) -> Result<Self, Error> {
        offer.validate()?;
        if timeout_us == 0 || timeout_us > MAX_TIMEOUT_US {
            return Err(Error::Clock);
        }
        let until_us = now_us.checked_add(timeout_us).ok_or(Error::Clock)?;
        let maximum =
            (offer.limits.max_control_message_bytes() as usize).min(negotiation::MAX_RECORD);
        let mut this = Self {
            offer,
            selection: None,
            opened: None,
            approval: None,
            inspection: false,
            inspected: None,
            directory: false,
            assembler: None,
            listed: None,
            created_us: now_us,
            phase: Phase::Hello,
            bytes: [0; negotiation::MAX_RECORD],
            len: 0,
            maximum,
            last_us: now_us,
            until_us,
        };
        this.len = negotiation::encode(
            &Message::ClientHello(this.offer.clone()),
            maximum,
            &mut this.bytes,
        )?;
        Ok(this)
    }
    /// Arm terminal inspection before the original `ClientHello` is sent. The
    /// caller must have prepared its required marker in that original offer.
    /// The two-second budget includes time parked since this owner's creation.
    pub fn begin_inspection(&mut self, now_us: u64) -> Result<(), Error> {
        self.tick(now_us)?;
        if self.phase != Phase::Hello || self.inspection || !inspection::requested(&self.offer) {
            self.close();
            return Err(Error::Order);
        }
        self.inspection = true;
        self.until_us = self
            .until_us
            .min(self.created_us.saturating_add(inspection::MAX_DURATION_US));
        self.tick(now_us)
    }
    /// Arm a terminal directory request before the original `ClientHello` is
    /// sent; the offer must carry the required directory marker. The budget
    /// includes time parked since this owner's creation.
    pub fn begin_directory(&mut self, now_us: u64) -> Result<(), Error> {
        self.tick(now_us)?;
        if self.phase != Phase::Hello
            || self.inspection
            || self.directory
            || !directory::requested(&self.offer)
        {
            self.close();
            return Err(Error::Order);
        }
        self.directory = true;
        self.until_us = self
            .until_us
            .min(self.created_us.saturating_add(directory::MAX_DURATION_US));
        self.tick(now_us)
    }
    /// Never refreshed by polling, packet arrival, or approval notifications.
    pub const fn deadline_us(&self) -> u64 {
        self.until_us
    }
    pub fn tick(&mut self, now_us: u64) -> Result<(), Error> {
        let error = if self.phase == Phase::Closed {
            Some(Error::Closed)
        } else if now_us < self.last_us {
            Some(Error::Clock)
        } else if now_us >= self.until_us {
            Some(if self.approval.is_some() {
                Error::ApprovalUnanswered
            } else {
                Error::Expired
            })
        } else {
            None
        };
        if let Some(error) = error {
            self.close();
            return Err(error);
        }
        self.last_us = now_us;
        Ok(())
    }
    /// Borrow the exact pending record. Backpressure does not consume it.
    pub fn pending(&mut self, now_us: u64) -> Result<Option<&[u8]>, Error> {
        self.tick(now_us)?;
        Ok((self.len != 0).then_some(&self.bytes[..self.len]))
    }
    /// The transport has accepted this record. It must retain the original
    /// absolute deadline while completing partial writes/retransmissions.
    pub fn sent(&mut self, now_us: u64) -> Result<(), Error> {
        self.tick(now_us)?;
        self.phase = match (self.phase, self.len != 0) {
            (Phase::Hello, true) => Phase::Capabilities,
            (Phase::Selection, true) => Phase::Open,
            (Phase::Ack, true) => Phase::Complete,
            (Phase::InspectionClose, true) => Phase::InspectionClosed,
            (Phase::DirectoryRequest, true) => Phase::DirectoryListing,
            _ => {
                self.close();
                return Err(Error::Order);
            }
        };
        self.len = 0;
        Ok(())
    }
    /// Consume one complete record from the installed initial control pair.
    /// Any protocol/order error closes the startup rather than retrying it.
    pub fn receive(&mut self, bytes: &[u8], now_us: u64) -> Result<(), Error> {
        self.tick(now_us)?;
        let result = self.receive_inner(bytes);
        if result.is_err() {
            self.close();
        }
        result
    }
    fn receive_inner(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if self.len != 0 {
            return Err(Error::Order);
        }
        if self.phase == Phase::InspectionClosed {
            inspection::decode_closed(bytes, self.maximum).map_err(negotiation::Error::from)?;
            self.phase = Phase::Inspected;
            return Ok(());
        }
        if self.phase == Phase::DirectoryListing {
            return self.receive_listing(bytes);
        }
        let message = negotiation::decode(bytes, self.maximum, 0)?;
        match (self.phase, message) {
            (Phase::Capabilities, Message::HostCapabilities(host)) => {
                self.offer.check_host(&host)?;
                if self.inspection {
                    if !inspection::requested(&host) {
                        return Err(negotiation::Error::RequiredCapability.into());
                    }
                    self.maximum = (host.limits.max_control_message_bytes() as usize)
                        .min(negotiation::MAX_RECORD);
                    self.len = inspection::encode_request(self.maximum, &mut self.bytes)
                        .map_err(negotiation::Error::from)?;
                    self.inspected = Some(host);
                    self.phase = Phase::InspectionClose;
                    return Ok(());
                }
                if self.directory {
                    if !directory::requested(&host) {
                        return Err(negotiation::Error::RequiredCapability.into());
                    }
                    self.maximum = (host.limits.max_control_message_bytes() as usize)
                        .min(negotiation::MAX_RECORD);
                    self.len = directory::encode_request(self.maximum, &mut self.bytes)
                        .map_err(negotiation::Error::from)?;
                    self.phase = Phase::DirectoryRequest;
                    return Ok(());
                }
                let selection = host.select()?;
                self.maximum = (selection.limits.max_control_message_bytes() as usize)
                    .min(negotiation::MAX_RECORD);
                self.len = negotiation::encode(
                    &Message::SelectedConfiguration(selection.clone()),
                    self.maximum,
                    &mut self.bytes,
                )?;
                self.selection = Some(selection);
                self.phase = Phase::Selection;
            }
            (
                Phase::Open,
                Message::ApprovalRequired {
                    request,
                    deadline_us,
                    role,
                },
            ) => {
                if self.approval.is_some() || role != self.offer.role || deadline_us == 0 {
                    return Err(Error::Order);
                }
                self.approval = Some(ApprovalNotice {
                    request,
                    host_deadline_us: deadline_us,
                });
            }
            (
                Phase::Open,
                Message::SessionOpened {
                    binding,
                    selection,
                    observation_until_us,
                },
            ) => {
                if self.selection.as_ref() != Some(&selection)
                    || observation_until_us == 0
                    || self
                        .approval
                        .is_some_and(|a| a.request != binding.remote_session)
                {
                    return Err(Error::Order);
                }
                self.opened = Some(Opened {
                    binding,
                    selection,
                    observation_until_us,
                });
                self.phase = Phase::Bind;
            }
            _ => return Err(Error::Order),
        }
        Ok(())
    }
    /// One directory listing record, in order; the final one completes it.
    fn receive_listing(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if bytes.get(6..8) != Some(&(Kind::DirectoryListing as u16).to_be_bytes()[..]) {
            // The only other record is the host's typed refusal.
            negotiation::decode(bytes, self.maximum, 0)?;
            return Err(Error::Order);
        }
        let assembler = self.assembler.get_or_insert_with(Assembler::default);
        if assembler
            .push(bytes, self.maximum)
            .map_err(negotiation::Error::from)?
        {
            let listing = self
                .assembler
                .take()
                .ok_or(Error::Order)?
                .finish()
                .map_err(negotiation::Error::from)?;
            self.listed = Some(listing);
            self.phase = Phase::Listed;
        }
        Ok(())
    }
    pub fn approval(&self) -> Option<ApprovalNotice> {
        self.approval
    }
    /// Install this binding on the SAME transport streams, only after all
    /// `SelectedConfiguration` bytes have been staged. This does not attach media.
    pub fn binding_to_install(
        &mut self,
        now_us: u64,
    ) -> Result<Option<(ControlBinding, usize)>, Error> {
        self.tick(now_us)?;
        Ok(if self.phase == Phase::Bind {
            Some((
                self.opened.as_ref().ok_or(Error::Order)?.binding,
                self.maximum,
            ))
        } else {
            None
        })
    }
    pub fn bound(&mut self, binding: u32, now_us: u64) -> Result<(), Error> {
        self.tick(now_us)?;
        if self.phase != Phase::Bind || self.opened.as_ref().is_none_or(|o| o.binding.id != binding)
        {
            self.close();
            return Err(Error::Order);
        }
        self.len = match negotiation::encode(
            &Message::BindingAccepted { binding },
            self.maximum,
            &mut self.bytes,
        ) {
            Ok(n) => n,
            Err(e) => {
                self.close();
                return Err(e.into());
            }
        };
        self.phase = Phase::Ack;
        Ok(())
    }
    /// Only true after the binding acknowledgement enters transport ownership.
    /// The remote host still has to receive it before enabling observation.
    pub fn is_complete(&self) -> bool {
        self.phase == Phase::Complete
    }
    pub fn finish(mut self, now_us: u64) -> Result<Opened, Error> {
        self.tick(now_us)?;
        if self.phase != Phase::Complete {
            return Err(Error::Order);
        }
        self.opened.take().ok_or(Error::Order)
    }
    /// True only after the host's exact terminal acknowledgement. This is
    /// separate from a selected configuration or an observation-capable session.
    pub fn is_inspected(&self) -> bool {
        self.phase == Phase::Inspected
    }
    pub fn finish_inspection(mut self, now_us: u64) -> Result<Offer, Error> {
        self.tick(now_us)?;
        if self.phase != Phase::Inspected {
            return Err(Error::Order);
        }
        self.inspected.take().ok_or(Error::Order)
    }
    /// Waiting for (more) directory listing records.
    pub fn is_listing(&self) -> bool {
        self.phase == Phase::DirectoryListing && self.len == 0
    }
    /// True only after the host's complete listing (its final record).
    pub fn is_listed(&self) -> bool {
        self.phase == Phase::Listed
    }
    pub fn finish_directory(mut self, now_us: u64) -> Result<Listing, Error> {
        self.tick(now_us)?;
        if self.phase != Phase::Listed {
            return Err(Error::Order);
        }
        self.listed.take().ok_or(Error::Order)
    }
    pub fn close(&mut self) {
        self.phase = Phase::Closed;
        self.len = 0;
        self.opened = None;
        self.selection = None;
        self.approval = None;
        self.inspected = None;
        self.assembler = None;
        self.listed = None;
        self.bytes.fill(0);
    }
}
