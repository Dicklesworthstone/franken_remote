//! Revocation-aware native handoffs. No payload or native API is owned here.
use super::{Epoch, Error};
use crate::{clipboard::ProjectedClock, input::ClientInstant};
use fr_core::audio::AudioStreamConfig;
use std::sync::{Arc, Weak};

/// Only an exact accepted host acknowledgement mints this original-epoch
/// permit. Workers may share it through Arc; it cannot keep its Sender or the
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
        let epoch = self.epoch.upgrade().ok_or(Error::Retired)?;
        if !epoch.is_live() {
            return Err(Error::Retired);
        }
        if now >= self.until {
            epoch.stop();
            return Err(Error::Expired);
        }
        let (_, effective) = match self.clock.sample_concurrent(now) {
            Ok(sample) => sample,
            Err(error) => {
                epoch.stop();
                return Err(Error::Authority(error));
            }
        };
        // Another original-owner/worker check may have overtaken the caller's
        // clock sample. A newer lease proof cannot revive this fixed permit cap.
        if effective >= self.until {
            epoch.stop();
            return Err(Error::Expired);
        }
        if !epoch.is_live() {
            return Err(Error::Retired);
        }
        Ok(())
    }
    pub(super) fn belongs_to(&self, epoch: &Arc<Epoch>) -> bool {
        self.epoch.ptr_eq(&Arc::downgrade(epoch)) && epoch.is_live()
    }
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
