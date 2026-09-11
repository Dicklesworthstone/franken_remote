#![forbid(unsafe_code)]
//! Exact change detection for the explicitly CPU-staged X11 capture path.
//!
//! Keep one last successfully encoded source plus at most one pending snapshot.
//! Compare full initialized BGRA bytes, never sampled pixels or collision-prone
//! hashes. A failed capture/encode is not unchanged evidence. The caller owns
//! capture cadence; each check still pays for an actual X11 readback.
use crate::{BgraFrame, HevcEncoder, NativeError, X11Surface};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId},
    worker::UnchangedCapture,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureOutput {
    Submitted,
    Unchanged(UnchangedCapture),
}
struct Snapshot {
    frame: FrameId,
    pixels: BgraFrame,
}
// Fixed bounded native inventory stays inline in the single worker owner.
#[allow(clippy::large_enum_variant)]
enum CaptureSurface {
    Root(X11Surface),
    #[cfg(feature = "linux-displays")]
    Selected(crate::displays::X11SelectedCapture),
}
impl CaptureSurface {
    fn verify(&mut self) -> Result<(), NativeError> {
        match self {
            Self::Root(surface) => surface.revalidate(),
            #[cfg(feature = "linux-displays")]
            Self::Selected(surface) => surface.revalidate(),
        }
    }
    fn snapshot(&mut self) -> Result<BgraFrame, NativeError> {
        match self {
            Self::Root(s) => s.snapshot(),
            #[cfg(feature = "linux-displays")]
            Self::Selected(s) => s.snapshot(),
        }
    }
}
pub struct ChangeAwareCapture {
    surface: CaptureSurface,
    codec: HevcEncoder,
    baseline: Option<Snapshot>,
    pending: Option<Snapshot>,
    last_request: Option<FrameId>,
    last_observed: Option<u64>,
    closed: bool,
}
impl ChangeAwareCapture {
    pub const fn new(surface: X11Surface, codec: HevcEncoder) -> Self {
        Self {
            surface: CaptureSurface::Root(surface),
            codec,
            baseline: None,
            pending: None,
            last_request: None,
            last_observed: None,
            closed: false,
        }
    }
    #[cfg(feature = "linux-displays")]
    pub const fn selected(
        surface: crate::displays::X11SelectedCapture,
        codec: HevcEncoder,
    ) -> Self {
        Self {
            surface: CaptureSurface::Selected(surface),
            codec,
            baseline: None,
            pending: None,
            last_request: None,
            last_observed: None,
            closed: false,
        }
    }
    /// Topology only: this does not update the last pixel/source observation.
    pub fn check_display(&mut self) -> Result<(), NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        match &mut self.surface {
            #[cfg(feature = "linux-displays")]
            CaptureSurface::Selected(surface) => {
                surface.revalidate().inspect_err(|_| self.closed = true)
            }
            CaptureSurface::Root(_) => Err(NativeError::Unavailable),
        }
    }
    /// The supplied timestamp is the parent's pre-request lower bound, NEVER
    /// the reply's arrival time. Force-IDR/refinement always bypasses suppression.
    pub fn capture(
        &mut self,
        frame: FrameId,
        observed_micros: u64,
        force_idr: bool,
        only_if_changed: bool,
    ) -> Result<CaptureOutput, NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        if self.pending.is_some() {
            return Err(NativeError::NeedDrain);
        }
        if self.last_request.is_some_and(|id| frame <= id)
            || self.last_observed.is_some_and(|t| observed_micros < t)
        {
            self.closed = true;
            return Err(NativeError::StaleGeneration);
        }
        self.last_request = Some(frame);
        self.last_observed = Some(observed_micros);
        // Nothing acquired below can bless the baseline until capture succeeds.
        let pixels = self
            .surface
            .snapshot()
            .inspect_err(|_| self.closed = true)?;
        if only_if_changed
            && !force_idr
            && let Some(old) = &self.baseline
            && old.pixels.width() == pixels.width()
            && old.pixels.height() == pixels.height()
            && old.pixels.pixels() == pixels.pixels()
        {
            self.surface.verify().inspect_err(|_| self.closed = true)?;
            return Ok(CaptureOutput::Unchanged(UnchangedCapture {
                candidate: frame,
                reference: old.frame,
                observed_micros,
            }));
        }
        self.codec
            .submit(&pixels, frame, observed_micros, force_idr)
            .inspect_err(|_| self.closed = true)?;
        self.pending = Some(Snapshot { frame, pixels });
        Ok(CaptureOutput::Submitted)
    }
    pub fn poll_output(&mut self) -> Result<EncodedAccessUnit, NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        if self.pending.is_none() {
            return Err(NativeError::NeedInput);
        }
        let unit = self.codec.poll_output().inspect_err(|error| {
            if *error != NativeError::NeedInput {
                self.closed = true;
            }
        })?;
        if self
            .pending
            .as_ref()
            .is_none_or(|p| p.frame != unit.frame())
        {
            self.closed = true;
            return Err(NativeError::UnsupportedBitstream);
        }
        // Encoding can block after capture validation. A changed selected output
        // must not release either an access unit or unchanged-source evidence.
        self.surface.verify().inspect_err(|_| self.closed = true)?;
        // Move the snapshot only AFTER the validated access unit exists. Pending
        // or refused native submissions cannot become an unchanged-source anchor.
        self.baseline = self.pending.take();
        Ok(unit)
    }
}
