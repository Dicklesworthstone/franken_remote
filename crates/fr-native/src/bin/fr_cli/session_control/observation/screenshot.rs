//! Export only an explicit read of the original streaming observation.
use super::{Phase, Publisher};
use crate::session_control::screenshot::{Metadata, Refusal};
use fr_client::robot::live::Evidence;
use fr_client::robot::snapshot::{Handle, Image};

impl Publisher {
    pub fn bind_snapshot(&self, handle: Handle) -> Result<(), ()> {
        let old = self.1.try_lock().map_err(|_| ())?.replace(handle);
        drop(old);
        Ok(())
    }
    pub(crate) async fn capture(&self) -> Result<(Metadata, Image), Refusal> {
        let original = self.read().ok_or(Refusal::Unavailable)?;
        let frame = original.frame.as_ref().ok_or(Refusal::NotReady)?;
        if original.phase != Phase::Streaming
            || frame.evidence_level != Evidence::SubmittedToCompositor
        {
            return Err(Refusal::NotReady);
        }
        let handle = self
            .1
            .try_lock()
            .map_err(|_| Refusal::Busy)?
            .as_ref()
            .cloned()
            .ok_or(Refusal::Unavailable)?;
        let stamp = handle.current_stamp().ok_or(Refusal::NotReady)?;
        if stamp.frame.as_raw() != frame.frame_serial {
            return Err(Refusal::NotCurrent);
        }
        let image = handle
            .request(stamp)
            .map_err(Refusal::from)?
            .await
            .map_err(Refusal::from)?;
        let current = self.read().ok_or(Refusal::Unavailable)?;
        if current.attempt != original.attempt
            || current.phase != Phase::Streaming
            || current.frame.as_ref().is_none_or(|now| {
                now.frame_serial != frame.frame_serial
                    || now.geometry_generation != frame.geometry_generation
                    || now.display_handle != frame.display_handle
                    || now.evidence_level != Evidence::SubmittedToCompositor
            })
        {
            return Err(Refusal::NotCurrent);
        }
        let metadata =
            Metadata::new(current, image.snapshot(), None).map_err(|_| Refusal::NotCurrent)?;
        Ok((metadata, image))
    }
    /// A delivered image is historical. New pictures are allowed; retirement,
    /// a new attempt or selected geometry cannot seal its pending socket reply.
    pub(crate) fn check_export(&self, metadata: &Metadata) -> Result<(), Refusal> {
        let current = self.read().ok_or(Refusal::Unavailable)?;
        let original = metadata
            .observation
            .frame
            .as_ref()
            .ok_or(Refusal::NotCurrent)?;
        if current.attempt != metadata.observation.attempt
            || current.phase != Phase::Streaming
            || current.frame.as_ref().is_none_or(|frame| {
                frame.geometry_generation != original.geometry_generation
                    || frame.display_handle != original.display_handle
            })
        {
            return Err(Refusal::NotCurrent);
        }
        if metadata.control.is_some() {
            return Err(Refusal::Unavailable);
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "screenshot_tests.rs"]
mod tests;
