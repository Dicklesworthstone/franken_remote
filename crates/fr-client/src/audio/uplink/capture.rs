//! Revocation-aware native handoffs. No payload or native API is owned here.
use super::{Epoch, Error};
use crate::{clipboard::ProjectedClock, input::ClientInstant};
use fr_core::audio::AudioStreamConfig;
use std::sync::{Arc, Weak};

/// A reliable configuration record's immutable original authority. This is
/// deliberately distinct from `CapturePermit`: it cannot open a microphone,
/// submit PCM, or stand in for the exact positive host acknowledgement.
/// Retain it until the original native reliable send/retransmit queue drains.
#[derive(Clone)]
pub struct ConfigurationPermit {
    pub(super) epoch: Weak<Epoch>,
    pub(super) clock: ProjectedClock,
    pub(super) until: ClientInstant,
}
impl std::fmt::Debug for ConfigurationPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneConfigurationPermit")
            .field("until", &self.until)
            .finish_non_exhaustive()
    }
}
impl ConfigurationPermit {
    pub const fn local_deadline(&self) -> ClientInstant {
        self.until
    }
    pub fn check(&self, now: ClientInstant) -> Result<(), Error> {
        let effective = sample(&self.epoch, &self.clock, now)?;
        if effective >= self.until {
            return Err(Error::Expired);
        }
        Ok(())
    }
    pub fn valid(&self, now: ClientInstant) -> bool {
        self.check(now).is_ok()
    }
}

/// An exact accepted host acknowledgement opens this original talk epoch.
/// Explicit same-owner refreshes mint new immutable proofs within that epoch.
/// Workers may share a proof through `Arc`; it cannot keep its `Sender` or the
/// accepted controller alive, and its fixed authorization cap never renews.
pub struct CapturePermit {
    pub(super) epoch: Weak<Epoch>,
    pub(super) clock: ProjectedClock,
    pub(super) configuration: AudioStreamConfig,
    pub(super) until: ClientInstant,
}
impl std::fmt::Debug for CapturePermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneCapturePermit")
            .field("until", &self.until)
            .finish_non_exhaustive()
    }
}
impl CapturePermit {
    pub const fn configuration(&self) -> AudioStreamConfig {
        self.configuration
    }
    pub const fn local_deadline(&self) -> ClientInstant {
        self.until
    }
    pub fn valid(&self, now: ClientInstant) -> bool {
        self.check(now).is_ok()
    }
    pub fn check(&self, now: ClientInstant) -> Result<(), Error> {
        let effective = sample(&self.epoch, &self.clock, now)?;
        if effective >= self.until {
            // A legitimately installed successor does not retime this proof.
            // Its caller refuses this item without revoking the newer proof.
            return Err(Error::Expired);
        }
        Ok(())
    }
    /// Original-epoch monitor for a supervisor, not an item/capture permit.
    /// It observes only explicit `Sender` refreshes and cannot perform a renewal.
    /// Retained audio must still carry its immutable `CaptureReceipt` deadline.
    pub fn monitor(&self) -> CaptureMonitor {
        CaptureMonitor {
            epoch: self.epoch.clone(),
            clock: self.clock.clone(),
        }
    }
    pub(super) fn belongs_to(&self, epoch: &Arc<Epoch>) -> bool {
        self.epoch.ptr_eq(&Arc::downgrade(epoch)) && epoch.is_live()
    }
}
/// Read-only original talk/owner lifetime for an independently serviced native
/// supervisor. A clone neither retains the owner nor renews its authority.
#[derive(Clone)]
pub struct CaptureMonitor {
    epoch: Weak<Epoch>,
    clock: ProjectedClock,
}
impl std::fmt::Debug for CaptureMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneCaptureMonitor")
            .finish_non_exhaustive()
    }
}
impl CaptureMonitor {
    pub fn check(&self, now: ClientInstant) -> Result<(), Error> {
        sample(&self.epoch, &self.clock, now).map(|_| ())
    }
    pub fn valid(&self, now: ClientInstant) -> bool {
        self.check(now).is_ok()
    }
}
fn sample(
    original: &Weak<Epoch>,
    clock: &ProjectedClock,
    now: ClientInstant,
) -> Result<ClientInstant, Error> {
    let epoch = original.upgrade().ok_or(Error::Retired)?;
    let check = |at| {
        if !epoch.is_live() {
            return Err(Error::Retired);
        }
        let until = epoch.until.lock().map_err(|_| Error::Retired)?;
        if at >= *until {
            epoch.stop();
            return Err(Error::Expired);
        }
        Ok(())
    };
    check(now)?;
    let (_, effective) = match clock.sample_concurrent(now) {
        Ok(sample) => sample,
        Err(error) => {
            epoch.stop();
            return Err(Error::Authority(error));
        }
    };
    // Check effective ordered time under the current proof lock. An older
    // sampled callback cannot borrow a later controller renewal after expiry.
    check(effective)?;
    Ok(effective)
}

/// One bounded native pull's original custody. Several consecutive packets may
/// share it, up to the capture bound; no later reply or queue admission resets it.
pub struct CaptureReceipt {
    pub(super) epoch: Weak<Epoch>,
    pub(super) acquired: ClientInstant,
    pub(super) expires: ClientInstant,
    pub(super) packets: u8,
}
impl std::fmt::Debug for CaptureReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneCaptureReceipt")
            .field("expires", &self.expires)
            .field("packets", &self.packets)
            .finish_non_exhaustive()
    }
}
impl CaptureReceipt {
    pub const fn acquired(&self) -> ClientInstant {
        self.acquired
    }
    pub const fn expires(&self) -> ClientInstant {
        self.expires
    }
    /// Preserve an older partial PCM deadline reported by the actual worker.
    /// This cannot extend even one already admitted packet's custody.
    pub fn restrict_expiry(&mut self, expires: ClientInstant) -> Result<(), Error> {
        if self.packets != 0 && expires != self.expires {
            if let Some(epoch) = self.epoch.upgrade() {
                epoch.stop();
            }
            return Err(Error::Order);
        }
        if expires > self.expires || expires <= self.acquired {
            if let Some(epoch) = self.epoch.upgrade() {
                epoch.stop();
            }
            return Err(Error::Expired);
        }
        self.expires = expires;
        Ok(())
    }
    pub(super) fn belongs_to(&self, epoch: &Arc<Epoch>) -> bool {
        self.epoch.ptr_eq(&Arc::downgrade(epoch)) && epoch.is_live()
    }
}
