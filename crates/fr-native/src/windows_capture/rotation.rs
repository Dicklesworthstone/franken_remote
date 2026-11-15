//! Checked geometry at the Desktop Duplication boundary. Display and input
//! coordinates are upright; the acquired image is in the output's original
//! orientation. A quarter turn swaps only the acquired texture's axes.
#![forbid(unsafe_code)]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    Identity,
    Clockwise90,
    Clockwise180,
    Clockwise270,
}

impl Rotation {
    pub const fn from_degrees(degrees: u32) -> Option<Self> {
        match degrees {
            0 => Some(Self::Identity),
            90 => Some(Self::Clockwise90),
            180 => Some(Self::Clockwise180),
            270 => Some(Self::Clockwise270),
            _ => None,
        }
    }

    pub const fn quarter_turns(self) -> u8 {
        match self {
            Self::Identity => 0,
            Self::Clockwise90 => 1,
            Self::Clockwise180 => 2,
            Self::Clockwise270 => 3,
        }
    }
}

/// The actual acquired texture description, checked before an input view or
/// copy can expose a mismatched or partially initialized resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Texture {
    pub size: (u32, u32),
    pub bgra: bool,
    pub mip_levels: u32,
    pub array_size: u32,
    pub sample_count: u32,
    pub sample_quality: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mismatch {
    Geometry,
    Resource,
}

/// One fixed capture owner's layout. The full owned BGRA desktop consumes
/// at most four times the admitted pixel budget, with no rotation scratch
/// surface. Both axes also fit D3D's signed source/destination rectangles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    upright: (u32, u32),
    rotation: Rotation,
}

impl Layout {
    pub fn new(
        upright: (u32, u32),
        degrees: u32,
        max_dimension: u32,
        max_pixels: u64,
    ) -> Option<Self> {
        let rotation = Rotation::from_degrees(degrees)?;
        let max_dimension = max_dimension.min(i32::MAX.cast_unsigned());
        if upright.0 == 0
            || upright.1 == 0
            || upright.0 > max_dimension
            || upright.1 > max_dimension
        {
            return None;
        }
        let pixels = u64::from(upright.0).checked_mul(u64::from(upright.1))?;
        if pixels > max_pixels || usize::try_from(pixels.checked_mul(4)?).is_err() {
            return None;
        }
        Some(Self { upright, rotation })
    }

    pub const fn rotation(self) -> Rotation {
        self.rotation
    }

    pub const fn upright(self) -> (u32, u32) {
        self.upright
    }

    pub const fn acquired(self) -> (u32, u32) {
        match self.rotation {
            Rotation::Identity | Rotation::Clockwise180 => self.upright,
            Rotation::Clockwise90 | Rotation::Clockwise270 => (self.upright.1, self.upright.0),
        }
    }

    /// Duplication mode dimensions describe the upright output; acquired
    /// texture dimensions are checked separately below. A different rotation
    /// cannot reuse this owner, even when the dimensions have not changed.
    pub fn matches_mode(self, upright: (u32, u32), rotation: Rotation) -> bool {
        self.upright == upright && self.rotation == rotation
    }

    pub fn check_texture(self, texture: Texture) -> Result<(), Mismatch> {
        if texture.size != self.acquired() {
            return Err(Mismatch::Geometry);
        }
        if !texture.bgra
            || texture.mip_levels != 1
            || texture.array_size != 1
            || texture.sample_count != 1
            || texture.sample_quality != 0
        {
            return Err(Mismatch::Resource);
        }
        Ok(())
    }
}
