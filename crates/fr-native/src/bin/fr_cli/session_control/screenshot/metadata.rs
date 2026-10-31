//! Content-free metadata bound to one actual exported viewport.
use super::Error;
use fr_client::robot::live::{Evidence, Phase, Snapshot as Observation};
use fr_media::worker::presentation_snapshot::{Snapshot, pixel_bytes};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Metadata {
    pub observation: Observation,
    pub configuration_generation: u64,
    pub recovery_generation: u64,
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub control: Option<crate::robot_input::Snapshot>,
}
impl Metadata {
    #[cfg(any(feature = "linux-desktop", test))]
    pub(crate) fn new(
        observation: Observation,
        image: &Snapshot,
        control: Option<crate::robot_input::Snapshot>,
    ) -> Result<Self, Error> {
        let value = Self {
            observation,
            configuration_generation: image.stamp().configuration.as_raw(),
            recovery_generation: image.stamp().recovery.as_raw(),
            viewport_width: image.width(),
            viewport_height: image.height(),
            control,
        };
        value.validate_image(image)?;
        Ok(value)
    }
    pub(crate) fn validate_fields(&self) -> Result<(), Error> {
        self.observation.validate().map_err(|_| Error::Protocol)?;
        let frame = self.observation.frame.as_ref().ok_or(Error::Protocol)?;
        if self.observation.phase != Phase::Streaming
            || frame.evidence_level != Evidence::SubmittedToCompositor
            || self.viewport_width > frame.pixel_width
            || self.viewport_height > frame.pixel_height
            || pixel_bytes(self.viewport_width, self.viewport_height).is_err()
        {
            return Err(Error::Protocol);
        }
        if let Some(control) = &self.control {
            super::super::input::validate_snapshot(control)?;
            if control.ready
                && (control.frame != Some(frame.frame_serial)
                    || control.geometry_generation != Some(frame.geometry_generation)
                    || control.configuration_generation != Some(self.configuration_generation)
                    || control.recovery_generation != Some(self.recovery_generation))
            {
                return Err(Error::Protocol);
            }
        }
        Ok(())
    }
    /// The declared frame dimensions, not only the global ceiling, bound the
    /// reader's allocation before it consumes any pixel body.
    pub(crate) fn validate_length(&self, length: usize) -> Result<(), Error> {
        self.validate_fields()?;
        let pixels =
            pixel_bytes(self.viewport_width, self.viewport_height).map_err(|_| Error::Protocol)?;
        if pixels.checked_add(fr_media::worker::presentation_snapshot::HEADER_BYTES) != Some(length)
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
    pub(crate) fn validate_image(&self, image: &Snapshot) -> Result<(), Error> {
        self.validate_fields()?;
        let frame = self.observation.frame.as_ref().ok_or(Error::Protocol)?;
        let stamp = image.stamp();
        if stamp.frame.as_raw() != frame.frame_serial
            || stamp.configuration.as_raw() != self.configuration_generation
            || stamp.recovery.as_raw() != self.recovery_generation
            || (image.width(), image.height()) != (self.viewport_width, self.viewport_height)
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
    /// Accumulate real elapsed time, including IPC and caller-side PNG writing.
    /// Expired input handles disappear; this never asks for a newer observation.
    pub(crate) fn elapse(&mut self, milliseconds: u64) -> Result<(), Error> {
        let frame = self.observation.frame.as_mut().ok_or(Error::Protocol)?;
        frame.receipt_age_ms = frame
            .receipt_age_ms
            .checked_add(milliseconds)
            .ok_or(Error::Protocol)?;
        if let Some(control) = &mut self.control
            && let Some(age) = control.source_age_upper_ms.as_mut()
        {
            *age = age.checked_add(milliseconds).ok_or(Error::Protocol)?;
            if *age >= 1000 {
                super::super::input::clear_snapshot(control);
            }
        }
        self.validate_fields()
    }
}
