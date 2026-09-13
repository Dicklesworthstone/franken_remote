//! Input vocabulary shared by the bounded wire codec and final submission owner.
//! Coordinates are signed host desktop pixels AFTER the acknowledged client
//! transform. No layout guessing, edge clamping, or implicit clipboard paste.
use crate::ids::{
    CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, InputTicketId,
    RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
};
use core::fmt;

mod text;
pub use text::{CommittedText, MAX_COMMITTED_TEXT_BYTES, TextError};

/// The complete input view binding. A generation from another view is not usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputView {
    pub geometry: DisplayGeometryGeneration,
    pub viewport: ViewportMappingGeneration,
    pub configuration: CodecConfigurationGeneration,
    pub recovery: RecoveryGeneration,
}

/// Credentials travel only over an authenticated, admitted input channel.
/// Debug intentionally omits session, lease and ticket material.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct InputCredentials {
    pub session: RemoteSessionId,
    pub lease: InputLeaseId,
    pub ticket: InputTicketId,
    pub view: InputView,
}
impl fmt::Debug for InputCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputCredentials")
            .field("view", &self.view)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopPoint {
    pub x: i32,
    pub y: i32,
}

/// One locally selected display/crop, with exclusive right and bottom edges.
/// Arithmetic is widened before addition, including negative monitor origins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputBounds {
    origin: DesktopPoint,
    width: u32,
    height: u32,
}
impl InputBounds {
    pub fn new(origin: DesktopPoint, width: u32, height: u32) -> Option<Self> {
        if width == 0
            || height == 0
            || i64::from(origin.x) + i64::from(width) - 1 > i64::from(i32::MAX)
            || i64::from(origin.y) + i64::from(height) - 1 > i64::from(i32::MAX)
        {
            return None;
        }
        Some(Self {
            origin,
            width,
            height,
        })
    }
    pub const fn origin(self) -> DesktopPoint {
        self.origin
    }
    pub const fn width(self) -> u32 {
        self.width
    }
    pub const fn height(self) -> u32 {
        self.height
    }
    pub fn contains(self, point: DesktopPoint) -> bool {
        let x = i64::from(point.x) - i64::from(self.origin.x);
        let y = i64::from(point.y) - i64::from(self.origin.y);
        x >= 0 && y >= 0 && x < i64::from(self.width) && y < i64::from(self.height)
    }
}

/// USB keyboard/keypad usage page 0x07, NOT a character or platform keycode.
/// This v0 subset excludes reserved/vendor usages; OS adapters must map physical
/// positions or refuse them, independently of committed Unicode support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalKey(u16);
impl PhysicalKey {
    pub const fn new(usage: u16) -> Option<Self> {
        if (usage >= 4 && usage <= 0xa4) || (usage >= 0xe0 && usage <= 0xe7) {
            Some(Self(usage))
        } else {
            None
        }
    }
    pub const fn usage(self) -> u16 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum KeyTransition {
    Release = 0,
    Press = 1,
    Repeat = 2,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PointerButton {
    Primary = 1,
    Secondary = 2,
    Middle = 3,
    Back = 4,
    Forward = 5,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ScrollUnit {
    Pixels = 0,
    Lines = 1,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PointerMode {
    Absolute = 0,
    Relative = 1,
}

/// No action payload, including physical keys and coordinates, is logged by Debug.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputEvent<'a> {
    Key {
        key: PhysicalKey,
        transition: KeyTransition,
    },
    Button {
        button: PointerButton,
        pressed: bool,
        position: DesktopPoint,
        barrier: u64,
    },
    Pointer {
        position: DesktopPoint,
    },
    Relative {
        mode_epoch: u64,
        cumulative_x: i64,
        cumulative_y: i64,
    },
    Scroll {
        position: DesktopPoint,
        barrier: u64,
        x: i32,
        y: i32,
        unit: ScrollUnit,
    },
    Text(&'a str),
    Mode {
        mode: PointerMode,
        epoch: u64,
    },
}
impl InputEvent<'_> {
    pub const fn is_pointer(self) -> bool {
        matches!(self, Self::Pointer { .. })
    }
    pub const fn name(self) -> &'static str {
        match self {
            Self::Key { .. } => "Key",
            Self::Button { .. } => "Button",
            Self::Pointer { .. } => "Pointer",
            Self::Relative { .. } => "Relative",
            Self::Scroll { .. } => "Scroll",
            Self::Text(_) => "Text",
            Self::Mode { .. } => "Mode",
        }
    }
}
impl fmt::Debug for InputEvent<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Reliable actions and replaceable pointer states use separate sequence spaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRequest<'a> {
    pub credentials: InputCredentials,
    pub sequence: u64,
    pub event: InputEvent<'a>,
}
