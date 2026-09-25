//! Registry lifetime owns revocation, while the OS task retains native custody.
//! Fixed slots reference actual publishers, not session-record permission bits.
use super::{ProcessGeneration, SessionRegistry};
use crate::media::shared_publisher::{self, Publication, Publisher};
use fr_core::ids::{
    CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, OsSessionId,
};
use fr_wire::decoder::Binding;
use std::sync::{Arc, Mutex, Weak};

/// Separate hard bound on shared sources, never an allocation from peer input.
pub const MAX_PUBLICATIONS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationError {
    Full,
    NotFound,
    WrongScope,
    AlreadyPublished,
    StaleRegistration,
    SequenceExhausted,
    Poisoned,
    Source(shared_publisher::Error),
}
impl std::fmt::Display for PublicationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for PublicationError {}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Scope {
    boot: HostBootId,
    os: OsSessionId,
    process: ProcessGeneration,
    geometry: DisplayGeometryGeneration,
    codec: CodecConfigurationGeneration,
}
impl Scope {
    fn matches(self, view: Binding) -> bool {
        (self.boot, self.os, self.geometry, self.codec)
            == (
                view.parent.host_boot,
                view.parent.os_session,
                view.geometry,
                view.configuration,
            )
    }
}
struct Entry {
    serial: u64,
    publication: Publication,
}
struct Sources {
    slots: [Option<Entry>; MAX_PUBLICATIONS],
    serial: u64,
}
impl Sources {
    fn clear(&mut self) {
        for slot in &mut self.slots {
            if let Some(entry) = slot.take() {
                entry.publication.revoke();
            }
        }
    }
}
/// This registry owns no media, worker, TLS key or new runtime. Native owners
/// must keep driving cancellation and retain their children until confirmed reap.
pub(super) struct Publications {
    sources: Arc<Mutex<Sources>>,
    identity: Arc<()>,
}
impl std::fmt::Debug for Publications {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Publications([bounded source lifetimes])")
    }
}
impl Publications {
    pub(super) fn new() -> Self {
        Self {
            identity: Arc::new(()),
            sources: Arc::new(Mutex::new(Sources {
                slots: core::array::from_fn(|_| None),
                serial: 0,
            })),
        }
    }
    pub(super) fn clear(&mut self) {
        self.sources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}
impl Drop for Publications {
    fn drop(&mut self) {
        self.clear();
    }
}
/// Local routing proof, NOT observation permission. It is weak and generation
/// specific; it cannot keep a registry/source alive or select a reused slot.
#[derive(Clone)]
pub struct RegisteredPublisher {
    sources: Weak<Mutex<Sources>>,
    slot: usize,
    serial: u64,
}
impl std::fmt::Debug for RegisteredPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredPublisher")
            .field("serial", &self.serial)
            .finish_non_exhaustive()
    }
}
impl RegisteredPublisher {
    fn with_source<T>(
        &self,
        operation: impl FnOnce(&Publication) -> Result<T, PublicationError>,
    ) -> Result<T, PublicationError> {
        let shared = self
            .sources
            .upgrade()
            .ok_or(PublicationError::StaleRegistration)?;
        let sources = shared.lock().map_err(|_| PublicationError::Poisoned)?;
        let entry = sources
            .slots
            .get(self.slot)
            .and_then(Option::as_ref)
            .filter(|entry| entry.serial == self.serial)
            .ok_or(PublicationError::StaleRegistration)?;
        entry
            .publication
            .check()
            .map_err(PublicationError::Source)?;
        operation(&entry.publication)
    }
    /// Hold the registry generation gate through admission. Revocation cannot
    /// race a lookup into authorizing a member on a retired source.
    pub(crate) fn admit(
        &self,
        control: crate::media::ObservationControl,
        media: crate::media_quic::NegotiatedMedia,
        q: &fr_transport::quic::QuicRecords,
        policy: fr_media::delivery::SendPolicy,
        timeout: std::time::Duration,
    ) -> Result<shared_publisher::Subscriber, PublicationError> {
        self.with_source(|source| {
            let expected = source.view();
            let view = media.binding();
            if (
                expected.parent.host_boot,
                expected.parent.os_session,
                expected.display,
                expected.geometry,
                expected.configuration,
            ) != (
                view.parent.host_boot,
                view.parent.os_session,
                view.display,
                view.geometry,
                view.configuration,
            ) {
                return Err(PublicationError::WrongScope);
            }
            source
                .queue()
                .map_err(PublicationError::Source)?
                .admit(control, media, q, policy, timeout)
                .map_err(PublicationError::Source)
        })
    }
    /// Checks original publisher consent and registry lifetime without renewing
    /// either. Numeric source identifiers alone never make this proof live.
    pub fn check(&self) -> Result<(), PublicationError> {
        self.with_source(|_| Ok(()))
    }
}
impl SessionRegistry {
    fn publication_scope(&self) -> Scope {
        Scope {
            boot: self.host_boot_id,
            os: self.os_session_id,
            process: self.process_generation,
            geometry: self.geometry_generation,
            codec: self.codec_generation,
        }
    }
    /// Register an actual admitted source on this OS session. Re-registering
    /// the SAME owner is idempotent; an equal-numbered replacement must first
    /// retire its predecessor. Registration never creates a worker or grant.
    pub fn register_publisher(
        &mut self,
        publisher: &Publisher,
    ) -> Result<RegisteredPublisher, PublicationError> {
        let publication = publisher.publication().map_err(PublicationError::Source)?;
        let view = publication.view();
        if !self.publication_scope().matches(view) {
            return Err(PublicationError::WrongScope);
        }
        let mut sources = self
            .publications
            .sources
            .lock()
            .map_err(|_| PublicationError::Poisoned)?;
        if let Some((slot, entry)) = sources
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.as_ref().map(|e| (i, e)))
            .find(|(_, e)| e.publication.view().display == view.display)
        {
            if !entry.publication.same_owner(&publication) {
                return Err(PublicationError::AlreadyPublished);
            }
            return Ok(RegisteredPublisher {
                sources: Arc::downgrade(&self.publications.sources),
                slot,
                serial: entry.serial,
            });
        }
        let slot = sources
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or(PublicationError::Full)?;
        let serial = sources
            .serial
            .checked_add(1)
            .ok_or(PublicationError::SequenceExhausted)?;
        publication
            .claim_registry(&self.publications.identity)
            .map_err(PublicationError::Source)?;
        sources.serial = serial;
        sources.slots[slot] = Some(Entry {
            serial,
            publication,
        });
        Ok(RegisteredPublisher {
            sources: Arc::downgrade(&self.publications.sources),
            slot,
            serial,
        })
    }
    /// Resolve installed media scope only. The connection's original consent and
    /// attachments are separately required at admission. Viewport, remote-session
    /// and recovery IDs belong to each viewer, not to shared source identity.
    pub fn publisher(&self, view: Binding) -> Result<RegisteredPublisher, PublicationError> {
        if !self.publication_scope().matches(view) {
            return Err(PublicationError::WrongScope);
        }
        let sources = self
            .publications
            .sources
            .lock()
            .map_err(|_| PublicationError::Poisoned)?;
        let (slot, entry) = sources
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.as_ref().map(|e| (i, e)))
            .find(|(_, e)| e.publication.view().display == view.display)
            .ok_or(PublicationError::NotFound)?;
        entry
            .publication
            .check()
            .map_err(PublicationError::Source)?;
        Ok(RegisteredPublisher {
            sources: Arc::downgrade(&self.publications.sources),
            slot,
            serial: entry.serial,
        })
    }
    /// Retire only the selected actual source and all its viewers. An old or
    /// foreign handle cannot revoke its successor. Revocation is synchronous;
    /// the original source task still drains and reaps the native child.
    pub fn retire_publisher(
        &mut self,
        publisher: &RegisteredPublisher,
    ) -> Result<(), PublicationError> {
        if !publisher
            .sources
            .ptr_eq(&Arc::downgrade(&self.publications.sources))
        {
            return Err(PublicationError::StaleRegistration);
        }
        let mut sources = self
            .publications
            .sources
            .lock()
            .map_err(|_| PublicationError::Poisoned)?;
        let slot = sources
            .slots
            .get_mut(publisher.slot)
            .ok_or(PublicationError::StaleRegistration)?;
        if slot.as_ref().is_none_or(|e| e.serial != publisher.serial) {
            return Err(PublicationError::StaleRegistration);
        }
        slot.take().expect("validated source").publication.revoke();
        Ok(())
    }
    /// Bounded retained registration count, not an encoder/capture activity claim.
    pub fn publication_count(&self) -> Result<usize, PublicationError> {
        Ok(self
            .publications
            .sources
            .lock()
            .map_err(|_| PublicationError::Poisoned)?
            .slots
            .iter()
            .flatten()
            .count())
    }
}
