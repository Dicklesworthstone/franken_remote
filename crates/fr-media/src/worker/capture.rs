//! Private discovery of full X11 screens, not a network display catalog.
//! The same child/X connection retains these identities until selection.
use super::{CONFIG_BYTES, Configuration, Error};
use core::fmt;
use fr_core::limits::ProtocolLimits;

pub const MAX_SCREENS: usize = 8;
pub const SCREEN_BYTES: usize = 20;
pub const MAX_CATALOG_BYTES: usize = 1 + MAX_SCREENS * SCREEN_BYTES;
pub const CONFIGURE_BYTES: usize = CONFIG_BYTES + SCREEN_BYTES;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Screen {
    pub index: u32,
    pub root: u64,
    pub width: u32,
    pub height: u32,
}
impl fmt::Debug for Screen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CaptureScreen([private native identity])")
    }
}
impl Screen {
    pub fn validate(self, limits: &ProtocolLimits) -> Result<(), Error> {
        if usize::try_from(self.index).map_err(|_| Error::ResourceLimit)? >= MAX_SCREENS
            || self.root == 0
            || self.root > u64::from(u32::MAX)
        {
            return Err(Error::Malformed);
        }
        limits
            .validate_coded_dimensions(self.width, self.height)
            .map_err(|_| Error::ResourceLimit)?;
        if self.width < 16
            || self.height < 16
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
        {
            return Err(Error::Unsupported);
        }
        Ok(())
    }
    fn write(self, b: &mut Vec<u8>) {
        b.extend_from_slice(&self.index.to_be_bytes());
        b.extend_from_slice(&self.root.to_be_bytes());
        b.extend_from_slice(&self.width.to_be_bytes());
        b.extend_from_slice(&self.height.to_be_bytes());
    }
    fn read(b: &[u8]) -> Result<Self, Error> {
        if b.len() != SCREEN_BYTES {
            return Err(Error::Malformed);
        }
        let s = Self {
            index: u32::from_be_bytes(b[..4].try_into().map_err(|_| Error::Malformed)?),
            root: u64::from_be_bytes(b[4..12].try_into().map_err(|_| Error::Malformed)?),
            width: u32::from_be_bytes(b[12..16].try_into().map_err(|_| Error::Malformed)?),
            height: u32::from_be_bytes(b[16..20].try_into().map_err(|_| Error::Malformed)?),
        };
        s.validate(&ProtocolLimits::ABSOLUTE)?;
        Ok(s)
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Screens {
    entries: [Screen; MAX_SCREENS],
    count: u8,
}
impl fmt::Debug for Screens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaptureScreens")
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
}
impl Screens {
    pub fn new(screens: &[Screen]) -> Result<Self, Error> {
        if screens.is_empty() || screens.len() > MAX_SCREENS {
            return Err(Error::ResourceLimit);
        }
        for (i, s) in screens.iter().enumerate() {
            s.validate(&ProtocolLimits::ABSOLUTE)?;
            if screens[..i]
                .iter()
                .any(|other| other.index == s.index || other.root == s.root)
            {
                return Err(Error::Malformed);
            }
        }
        let mut entries = [Screen::default(); MAX_SCREENS];
        entries[..screens.len()].copy_from_slice(screens);
        Ok(Self {
            entries,
            count: u8::try_from(screens.len()).map_err(|_| Error::ResourceLimit)?,
        })
    }
    pub fn entries(&self) -> &[Screen] {
        &self.entries[..usize::from(self.count)]
    }
    pub fn contains(&self, screen: Screen) -> bool {
        self.entries().contains(&screen)
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + self.entries().len() * SCREEN_BYTES);
        out.push(self.count);
        for s in self.entries() {
            s.write(&mut out);
        }
        out
    }
    pub fn decode(b: &[u8]) -> Result<Self, Error> {
        let count = usize::from(*b.first().ok_or(Error::Malformed)?);
        if !(1..=MAX_SCREENS).contains(&count) || b.len() != 1 + count * SCREEN_BYTES {
            return Err(Error::ResourceLimit);
        }
        let mut entries = [Screen::default(); MAX_SCREENS];
        for (entry, chunk) in entries[..count]
            .iter_mut()
            .zip(b[1..].as_chunks::<SCREEN_BYTES>().0)
        {
            *entry = Screen::read(chunk)?;
        }
        Self::new(&entries[..count])
    }
}
pub fn configure(configuration: Configuration, screen: Screen) -> Result<Vec<u8>, Error> {
    screen.validate(&configuration.limits()?)?;
    if configuration.width != screen.width || configuration.height != screen.height {
        return Err(Error::GeometryChanged);
    }
    let mut out = configuration.encode()?;
    out.try_reserve_exact(SCREEN_BYTES)
        .map_err(|_| Error::Allocation)?;
    screen.write(&mut out);
    Ok(out)
}
pub fn parse_configuration(b: &[u8]) -> Result<(Configuration, Screen), Error> {
    if b.len() != CONFIGURE_BYTES {
        return Err(Error::ResourceLimit);
    }
    let configuration = Configuration::decode(&b[..CONFIG_BYTES])?;
    let screen = Screen::read(&b[CONFIG_BYTES..])?;
    screen.validate(&configuration.limits()?)?;
    if configuration.width != screen.width || configuration.height != screen.height {
        return Err(Error::GeometryChanged);
    }
    Ok((configuration, screen))
}
