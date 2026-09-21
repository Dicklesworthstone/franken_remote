//! The local interactive-session owner renews its independently approved sources.
//! No method here is a network handler or permission probe. Platform adapters must
//! update the original permission owner from actual OS events/probes.
use super::{PermissionKind, SessionAgent};
use crate::{
    input_watchdog::StopReason,
    media::shared_publisher::{Publisher, consent::Renewal},
};
use fr_core::time::HostInstant;
use std::sync::Arc;

pub use crate::media::shared_publisher::consent::{Error, Status};
/// Bounds registrations, including weak registrations for retired publishers.
pub const MAX_SOURCES: usize = 8;

#[derive(Default)]
pub(super) struct Sources {
    entries: [Option<Entry>; MAX_SOURCES],
}
#[derive(Clone)]
enum Entry {
    Published(Arc<Renewal>),
    Preparing(Arc<prepare::Reservation>),
}
impl Entry {
    fn close(&self) {
        match self {
            Self::Published(entry) => entry.close(),
            Self::Preparing(entry) => entry.close(),
        }
    }
    fn is_closed(&self) -> bool {
        match self {
            Self::Published(entry) => entry.is_closed(),
            Self::Preparing(entry) => entry.is_closed(),
        }
    }
    fn published(&self) -> Option<&Arc<Renewal>> {
        match self {
            Self::Published(entry) => Some(entry),
            Self::Preparing(_) => None,
        }
    }
}
impl Sources {
    pub(super) fn close(&mut self) {
        for entry in self.entries.iter_mut().filter_map(Option::take) {
            entry.close();
        }
    }
}
impl Drop for Sources {
    fn drop(&mut self) {
        self.close();
    }
}
/// Fixed-space local maintenance outcomes, with no challenge material or pixels.
#[derive(Debug)]
pub struct Report {
    outcomes: [Option<Result<Status, Error>>; MAX_SOURCES],
}
impl Report {
    pub fn outcomes(&self) -> impl Iterator<Item = &Result<Status, Error>> {
        self.outcomes.iter().flatten()
    }
    pub fn next_deadline(&self) -> Option<HostInstant> {
        self.outcomes()
            .filter_map(|r| r.as_ref().ok().map(|s| s.next_check))
            .min()
    }
}
impl SessionAgent {
    /// Attach an ALREADY locally approved source/display to this exact agent.
    /// Invoke only at the original local sharing-policy decision, never upon a
    /// remote heartbeat or decoder report. This grants neither initial observation
    /// nor input. The source must have independent authority/context. A locally
    /// selected native source can attach BEFORE its first viewer; other sources
    /// require an existing admitted cohort. Permission renewal never extends the
    /// unused source's startup budget. Unknown/missing capture permission refuses
    /// without changing the publisher.
    ///
    /// The same source cannot attach twice or gain another network renewer. Slots
    /// and metadata are bounded; retired publishers are not kept alive. This
    /// agent's indicator, lock/session-change handling and drop fence the source.
    pub fn attach_shared_source(&mut self, publisher: &Publisher) -> Result<(), Error> {
        let mut sources = self.sources.lock().map_err(|_| Error::Poisoned)?;
        let slot = sources
            .entries
            .iter()
            .position(|entry| entry.as_ref().is_none_or(Entry::is_closed))
            .ok_or(Error::Full)?;
        let renewal = Renewal::attach(publisher, self)?;
        sources.entries[slot] = Some(Entry::Published(Arc::new(renewal)));
        Ok(())
    }
    /// Bounded maintenance on the LOCAL authority/event loop. Service no later
    /// than `Report::next_deadline` and immediately after a permission update. The
    /// event loop must also service local revocation; no media/IPC work occurs here.
    /// A stalled loop lets original source deadlines expire terminally.
    ///
    /// Supply the host's qualified unpredictable, non-reusing nonce source, not
    /// viewer bytes or a clock-derived value. It is called only when renewal is
    /// due and outside all policy locks. Only source observation renews: viewer
    /// leases, decoder/readiness state, input tickets and pixel ages never change.
    pub fn service_shared_sources(
        &mut self,
        mut fresh_nonce: impl FnMut() -> Result<u128, ()>,
    ) -> Result<Report, Error> {
        // Bounded aliases let the indicator revoke concurrently, even inside the
        // caller's entropy callback. No policy lock surrounds caller code.
        let entries = self
            .sources
            .lock()
            .map_err(|_| Error::Poisoned)?
            .entries
            .clone();
        let mut report = Report {
            outcomes: core::array::from_fn(|_| None),
        };
        for (result, entry) in report.outcomes.iter_mut().zip(entries) {
            if let Some(entry) = entry {
                *result = Some(match entry {
                    Entry::Published(entry) => entry.service(self, &mut fresh_nonce),
                    Entry::Preparing(entry) => entry.check(self),
                });
            }
        }
        Ok(report)
    }
    /// Feed the real platform's capture-permission-loss event to the original
    /// agent. Fence observation and every affected source BEFORE returning; native
    /// child reaping remains with each Publisher, outside the authority path.
    /// The caller must submit the returned held-input cleanup operations; their
    /// effects are not rolled back by revoking observation.
    pub fn on_screen_capture_revoked(
        &mut self,
        now: HostInstant,
    ) -> (
        super::ImmediateRevokeOutcome,
        Vec<fr_core::input_submission::Operation>,
    ) {
        self.permissions
            .on_permission_revoked(PermissionKind::ScreenCapture);
        self.immediate_revoke(now, StopReason::AuthorityEnded)
    }
}

mod startup;
pub use startup::StartError;

/// Shared capture, local consent and original viewer service in one lifetime.
pub mod desktop;

/// Supervised discovery/configuration/first capture under local consent.
pub mod prepare;
