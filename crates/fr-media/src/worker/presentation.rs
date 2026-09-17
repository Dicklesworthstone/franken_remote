//! Locally chosen presentation destination, carried only on parent-owned pipes.
//! A window ID is neither input authority nor proof of visible presentation.
use super::{Configuration, Error, Kind};
use crate::hevc::DecoderRecord;
use fr_core::limits::ProtocolLimits;

pub const TARGET_BYTES: usize = 12;

/// An existing, UI-owned X11 window on the launch's local display. The UI keeps
/// it alive until the worker has stopped. Never construct this from peer data
/// or decoder output. The native adapter independently checks its actual type,
/// visual, dimensions and visibility, and never destroys or raises this window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct X11Target {
    window: u32,
    width: u32,
    height: u32,
}
impl X11Target {
    pub fn new(window: u32, width: u32, height: u32) -> Result<Self, Error> {
        ProtocolLimits::ABSOLUTE
            .validate_coded_dimensions(width, height)
            .map_err(|_| Error::ResourceLimit)?;
        if window == 0
            || width < 16
            || height < 16
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
        {
            return Err(Error::Malformed);
        }
        Ok(Self {
            window,
            width,
            height,
        })
    }
    pub const fn window(self) -> u32 {
        self.window
    }
    pub const fn width(self) -> u32 {
        self.width
    }
    pub const fn height(self) -> u32 {
        self.height
    }
    fn check(self, configuration: Configuration) -> Result<(), Error> {
        if self.width != configuration.width || self.height != configuration.height {
            return Err(Error::GeometryChanged);
        }
        Ok(())
    }
    pub fn encode_decoder(
        self,
        configuration: Configuration,
        record: &DecoderRecord,
    ) -> Result<Vec<u8>, Error> {
        self.check(configuration)?;
        let mut body = configuration.encode_decoder(record)?;
        body.try_reserve_exact(TARGET_BYTES)
            .map_err(|_| Error::Allocation)?;
        body.extend_from_slice(&self.window.to_be_bytes());
        body.extend_from_slice(&self.width.to_be_bytes());
        body.extend_from_slice(&self.height.to_be_bytes());
        if !Kind::ConfigurePresentation.accepts_length(body.len(), &configuration.limits()?) {
            return Err(Error::ResourceLimit);
        }
        Ok(body)
    }
    pub fn decode_decoder(body: &[u8]) -> Result<(Configuration, &[u8], Self), Error> {
        if !Kind::ConfigurePresentation.accepts_length(body.len(), &ProtocolLimits::ABSOLUTE) {
            return Err(Error::ResourceLimit);
        }
        let (decoder, target) = body.split_at(body.len() - TARGET_BYTES);
        let (configuration, record) = Configuration::decode_decoder(decoder)?;
        let target = Self::new(
            u32::from_be_bytes(target[0..4].try_into().unwrap()),
            u32::from_be_bytes(target[4..8].try_into().unwrap()),
            u32::from_be_bytes(target[8..12].try_into().unwrap()),
        )?;
        target.check(configuration)?;
        Ok((configuration, record, target))
    }
}
