//! Locally chosen presentation destination, carried only on parent-owned pipes.
//! A window ID is neither input authority nor proof of visible presentation.
use super::{Configuration, Error, Kind};
use crate::hevc::DecoderRecord;
use fr_core::limits::ProtocolLimits;

pub const TARGET_BYTES: usize = 12;
/// UI-owned X11 destination plus the original display's pixel dimensions.
pub const MAPPED_TARGET_BYTES: usize = TARGET_BYTES + 8;

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

/// The original display-space mapping of a locally owned X11 window. Encoded
/// picture dimensions choose sampling only: they never move this rectangle or
/// redefine input/cursor coordinates after HEVC's even crop rounding.
///
/// The target cannot exceed the original display, while an admitted canonical
/// picture may be smaller than the target when a shared source was configured
/// for another viewer. The renderer must use its explicit bounded sampling
/// policy; constructing this value is neither visibility nor input authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappedX11Target {
    target: X11Target,
    display_width: u32,
    display_height: u32,
    fit: Fit,
}
impl MappedX11Target {
    pub fn new(target: X11Target, display_width: u32, display_height: u32) -> Result<Self, Error> {
        // The original display can exceed the negotiated decoder ceiling, but
        // remains bounded by the implementation's absolute dimensions/pixels.
        ProtocolLimits::ABSOLUTE
            .validate_coded_dimensions(display_width, display_height)
            .map_err(|_| Error::ResourceLimit)?;
        if display_width < 16
            || display_height < 16
            || target.width > display_width
            || target.height > display_height
        {
            return Err(Error::GeometryChanged);
        }
        // A real display may have odd dimensions. Only its encoded HEVC crop
        // and the local X11 target require even pixels, not this source scope.
        let fit = Fit::place(display_width, display_height, target)?;
        Ok(Self {
            target,
            display_width,
            display_height,
            fit,
        })
    }
    pub const fn target(self) -> X11Target {
        self.target
    }
    pub const fn display_size(self) -> (u32, u32) {
        (self.display_width, self.display_height)
    }
    pub const fn fit(self) -> Fit {
        self.fit
    }
    /// Recheck every decoded picture against the original full-display scope
    /// and the caller's negotiated limits before allocation or native work.
    pub fn validate_picture(
        self,
        width: u32,
        height: u32,
        limits: &ProtocolLimits,
    ) -> Result<(), Error> {
        limits
            .validate_coded_dimensions(width, height)
            .and_then(|_| limits.validate_coded_dimensions(self.target.width, self.target.height))
            .map_err(|_| Error::ResourceLimit)?;
        if width < 16
            || height < 16
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
            || !fr_wire::display::picture::is_picture(self.display_size(), (width, height))
        {
            return Err(Error::GeometryChanged);
        }
        Ok(())
    }
    /// Distinct local IPC: a legacy worker must refuse instead of applying a
    /// native-pixel or encoded-picture-aspect mapping to the same window.
    pub fn encode_decoder(
        self,
        configuration: Configuration,
        record: &DecoderRecord,
    ) -> Result<Vec<u8>, Error> {
        let limits = configuration.limits()?;
        self.validate_picture(configuration.width, configuration.height, &limits)?;
        let mut body = configuration.encode_decoder(record)?;
        body.try_reserve_exact(MAPPED_TARGET_BYTES)
            .map_err(|_| Error::Allocation)?;
        for value in [
            self.target.window,
            self.target.width,
            self.target.height,
            self.display_width,
            self.display_height,
        ] {
            body.extend_from_slice(&value.to_be_bytes());
        }
        if !Kind::ConfigureMappedPresentation.accepts_length(body.len(), &limits) {
            return Err(Error::ResourceLimit);
        }
        Ok(body)
    }
    pub fn decode_decoder(body: &[u8]) -> Result<(Configuration, &[u8], Self), Error> {
        if !Kind::ConfigureMappedPresentation.accepts_length(body.len(), &ProtocolLimits::ABSOLUTE)
        {
            return Err(Error::ResourceLimit);
        }
        let (decoder, mapping) = body.split_at(body.len() - MAPPED_TARGET_BYTES);
        let (configuration, record) = Configuration::decode_decoder(decoder)?;
        // The fixed trailer was length-checked before slicing or allocation.
        let value = |offset| {
            u32::from_be_bytes(
                mapping[offset..offset + 4]
                    .try_into()
                    .expect("fixed mapping"),
            )
        };
        let target = Self::new(
            X11Target::new(value(0), value(4), value(8))?,
            value(12),
            value(16),
        )?;
        target.validate_picture(
            configuration.width,
            configuration.height,
            &configuration.limits()?,
        )?;
        Ok((configuration, record, target))
    }
}

/// Integer, half-open placement of the full remote image in a local window.
/// This is a local rendering choice, not a changed remote display generation or
/// input grant. The same rectangle must be used by the input viewport. Scaling
/// is downwards only and never changes the decoder's original dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fit {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}
impl Fit {
    pub fn new(source_width: u32, source_height: u32, target: X11Target) -> Result<Self, Error> {
        ProtocolLimits::ABSOLUTE
            .validate_coded_dimensions(source_width, source_height)
            .map_err(|_| Error::ResourceLimit)?;
        if source_width < 16
            || source_height < 16
            || !source_width.is_multiple_of(2)
            || !source_height.is_multiple_of(2)
            || target.width > source_width
            || target.height > source_height
        {
            return Err(Error::GeometryChanged);
        }
        Self::place(source_width, source_height, target)
    }
    /// Both callers have validated the source and the destination bounds.
    fn place(source_width: u32, source_height: u32, target: X11Target) -> Result<Self, Error> {
        let (sw, sh) = (u64::from(source_width), u64::from(source_height));
        let (tw, th) = (u64::from(target.width), u64::from(target.height));
        let (width, height) = if tw * sh <= th * sw {
            (tw, tw * sh / sw)
        } else {
            (th * sw / sh, th)
        };
        if width == 0 || height == 0 {
            return Err(Error::GeometryChanged);
        }
        // All products are below the checked u32 dimension ceiling squared;
        // outputs cannot exceed the already validated target dimensions.
        Ok(Self {
            x: u32::try_from((tw - width) / 2).map_err(|_| Error::ResourceLimit)?,
            y: u32::try_from((th - height) / 2).map_err(|_| Error::ResourceLimit)?,
            width: u32::try_from(width).map_err(|_| Error::ResourceLimit)?,
            height: u32::try_from(height).map_err(|_| Error::ResourceLimit)?,
        })
    }
}
impl X11Target {
    /// Explicit local fit mode uses a different IPC kind. Older workers refuse
    /// it rather than interpreting a resized image as a native-pixel mapping.
    pub fn encode_fitted_decoder(
        self,
        configuration: Configuration,
        record: &DecoderRecord,
    ) -> Result<Vec<u8>, Error> {
        Fit::new(configuration.width, configuration.height, self)?;
        configuration
            .limits()?
            .validate_coded_dimensions(self.width, self.height)
            .map_err(|_| Error::ResourceLimit)?;
        let mut body = configuration.encode_decoder(record)?;
        body.try_reserve_exact(TARGET_BYTES)
            .map_err(|_| Error::Allocation)?;
        body.extend_from_slice(&self.window.to_be_bytes());
        body.extend_from_slice(&self.width.to_be_bytes());
        body.extend_from_slice(&self.height.to_be_bytes());
        if !Kind::ConfigureFittedPresentation.accepts_length(body.len(), &configuration.limits()?) {
            return Err(Error::ResourceLimit);
        }
        Ok(body)
    }
    pub fn decode_fitted_decoder(body: &[u8]) -> Result<(Configuration, &[u8], Self), Error> {
        if !Kind::ConfigureFittedPresentation.accepts_length(body.len(), &ProtocolLimits::ABSOLUTE)
        {
            return Err(Error::ResourceLimit);
        }
        let (decoder, target) = body.split_at(body.len() - TARGET_BYTES);
        let (configuration, record) = Configuration::decode_decoder(decoder)?;
        let target = Self::new(
            u32::from_be_bytes(target[0..4].try_into().unwrap()),
            u32::from_be_bytes(target[4..8].try_into().unwrap()),
            u32::from_be_bytes(target[8..12].try_into().unwrap()),
        )?;
        Fit::new(configuration.width, configuration.height, target)?;
        configuration
            .limits()?
            .validate_coded_dimensions(target.width, target.height)
            .map_err(|_| Error::ResourceLimit)?;
        Ok((configuration, record, target))
    }
}
