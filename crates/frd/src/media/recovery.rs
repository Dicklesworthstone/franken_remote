//! Reference failure joins the original authority, subscription and native source.
use super::{CaptureSource, Error, Subscription, host_now};
use fr_core::{authority::AuthorityError, time::HostInstant};
use fr_media::delivery::RecoveryDisposition;
use fr_wire::decoder::Binding;
use std::sync::Arc;

impl Subscription {
    /// Called by the admitted control route after negotiating reference-recovery.
    /// `binding` is the INSTALLED full view binding, never a peer-proposed tuple.
    /// The subscription must already have consumed this actual source's output;
    /// equal numeric frame/configuration IDs cannot select another native worker.
    ///
    /// Acceptance fences old media and shared native input authority BEFORE any
    /// codec work. The existing capture loop services the source's single IDR
    /// queue; it must also wake at `CaptureSource::next_recovery_deadline` when
    /// otherwise idle. Duplicates return false and cannot renew work or time.
    ///
    /// This does not grant input, claim presentation, change healthy viewers,
    /// or install fresh channels. The session still admits new bindings and runs
    /// decoder startup before calling `recover` and accepting the resulting IDR.
    pub fn request_recovery(
        &mut self,
        source: &mut CaptureSource,
        bytes: &[u8],
        binding: Binding,
    ) -> Result<bool, Error> {
        self.control.check()?;
        if source.configuration.generation != self.epoch.configuration
            || self
                .capture_source
                .as_ref()
                .is_none_or(|id| !Arc::ptr_eq(id, &source.source))
            || source
                .selected_control
                .as_ref()
                .is_some_and(|control| !Arc::ptr_eq(&control.authority, &self.control.authority))
        {
            return Err(Error::InvalidFrame);
        }
        // Pure authority and bounded parsing only. Never hold this lock through
        // scheduling, native IPC, cleanup acknowledgement or an await point.
        let (disposition, now) = {
            let mut authority = self.control.authority.lock().map_err(|_| Error::Poisoned)?;
            if authority.session() != binding.parent.remote_session {
                return Err(Error::Authority(AuthorityError::StaleLease));
            }
            let now = host_now(&self.control.cx)?;
            authority
                .authorize_observation_delivery(now)
                .map_err(Error::Authority)?;
            let disposition = self
                .cache
                .request_recovery(bytes, binding, now.as_micros())?;
            authority.mark_view_stale();
            (disposition, now)
        };
        match disposition {
            RecoveryDisposition::Accepted(demand) => {
                source
                    .recovery
                    .queue(&self.cache, demand, now.as_micros())
                    .map_err(Error::Receiver)?;
                Ok(true)
            }
            RecoveryDisposition::Coalesced => Ok(false),
        }
    }
}
impl CaptureSource {
    /// The original encoder lifetime owns this one queue and its 500 ms rate
    /// allowance. Generation replacement never recreates or refills it.
    pub fn next_recovery_deadline(&self) -> Option<HostInstant> {
        self.recovery.next_deadline().map(HostInstant::from_micros)
    }
}
