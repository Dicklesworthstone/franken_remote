//! Original-controller authority for an explicitly approved file receive lane.
//!
//! This module performs no filesystem work. Numeric wire IDs cannot create its
//! opaque handoff: only the accepted control owner can issue it, exactly once.
pub use crate::clipboard::projection::files::ControllerAuthority;
use crate::input::{ClientInstant, InputClient};
use fr_core::ids::InputLeaseId;
use fr_wire::negotiation::ControlBinding;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NotGranted,
    AlreadyAttached,
    WrongChannel,
    NotReady,
    Expired,
    Clock,
    Stopped,
}
impl From<crate::clipboard::Error> for Error {
    fn from(error: crate::clipboard::Error) -> Self {
        match error {
            crate::clipboard::Error::NotGranted => Self::NotGranted,
            crate::clipboard::Error::AlreadyAttached => Self::AlreadyAttached,
            crate::clipboard::Error::WrongChannel => Self::WrongChannel,
            crate::clipboard::Error::NotReady => Self::NotReady,
            crate::clipboard::Error::Expired => Self::Expired,
            crate::clipboard::Error::Clock => Self::Clock,
            _ => Self::Stopped,
        }
    }
}
impl InputClient {
    /// Join the completed Files route to the exact accepted grant. Callers must
    /// separately retain/check that authenticated route, positive download
    /// selection and local destination approval. This handoff cannot grant them.
    /// A failed or dropped receive owner cannot reacquire this lane in the lease.
    pub fn file_receive_authority(
        &mut self,
        parent: ControlBinding,
        channel: u32,
        lease: InputLeaseId,
        now: ClientInstant,
    ) -> Result<ControllerAuthority, Error> {
        self.tick(now).map_err(|_| Error::Stopped)?;
        let projection = self.files_projection.as_mut().ok_or(Error::NotGranted)?;
        if parent != projection.parent() || lease != projection.binding().lease {
            return Err(Error::WrongChannel);
        }
        projection.attach_files(channel, now).map_err(Into::into)
    }
}
