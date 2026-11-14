//! Source evidence for the current Desktop Duplication instance. Retained
//! GPU pixels are usable only after that instance supplied a desktop image
//! and its frame was successfully released. Recreating the interface alone
//! cannot make pixels from the previous instance current again.

/// What the current duplication established for one capture request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Observation {
    Image,
    Unchanged,
    Unknown,
}

/// Whether the capture request can produce pixels or source verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Capture {
    Encode,
    Unchanged,
    Refuse,
}

impl Observation {
    /// Forced recovery, an unconditional capture or a changed pointer can
    /// request a picture, but none can substitute for source evidence.
    pub(super) const fn capture(self, picture_requested: bool) -> Capture {
        match self {
            Self::Unknown => Capture::Refuse,
            Self::Image => Capture::Encode,
            Self::Unchanged if picture_requested => Capture::Encode,
            Self::Unchanged => Capture::Unchanged,
        }
    }
}

/// Result of preparing an acquired frame, before releasing it to DXGI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Frame {
    Image,
    PointerOnly,
    Failed,
}

#[derive(Debug, Default)]
pub(super) struct Source {
    image: bool,
}

impl Source {
    pub(super) const fn has_image(&self) -> bool {
        self.image
    }

    /// Invalidate before abandoning a lost duplication, including when
    /// `ReleaseFrame` reports that loss after a successful GPU copy.
    pub(super) fn invalidate(&mut self) {
        self.image = false;
    }

    /// A timeout or pointer-only callback is meaningful unchanged evidence
    /// only when this very duplication has already supplied pixels.
    pub(super) const fn unchanged(&self) -> Observation {
        if self.image {
            Observation::Unchanged
        } else {
            Observation::Unknown
        }
    }

    /// Commit evidence only after both copying and releasing succeed. An
    /// acquired pointer update never initializes or repairs desktop pixels.
    pub(super) fn released(&mut self, frame: Frame, success: bool) -> Observation {
        match (frame, success) {
            (Frame::Image, true) => {
                self.image = true;
                Observation::Image
            }
            (Frame::PointerOnly, true) => self.unchanged(),
            _ => {
                self.invalidate();
                Observation::Unknown
            }
        }
    }
}
