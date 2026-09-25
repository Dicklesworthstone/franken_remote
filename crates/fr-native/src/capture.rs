#![forbid(unsafe_code)]
//! Exact change detection for the explicitly CPU-staged X11 capture path.
//!
//! Keep one last successfully encoded source plus at most one pending snapshot.
//! Compare full initialized BGRA bytes, never sampled pixels or collision-prone
//! hashes. A failed capture/encode is not unchanged evidence. The caller owns
//! capture cadence. Optional DAMAGE observations avoid redundant readbacks of
//! the CPU-staged X drawable; a full verification readback remains mandatory
//! every 250 ms and on every force-IDR/unconditional request.
use crate::{BgraFrame, HevcEncoder, NativeError, X11Surface};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId},
    worker::UnchangedCapture,
};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureOutput {
    Submitted,
    Unchanged(UnchangedCapture),
}
/// Bounded counters for the actual native work, not estimated saved GPU time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureStats {
    pub readbacks: u64,
    pub damage_observations: u64,
    pub encoded_submissions: u64,
}
const VERIFICATION_INTERVAL: Duration = Duration::from_millis(250);
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
    fn enable_damage(&mut self) -> Result<bool, NativeError> {
        match self {
            Self::Root(surface) => surface.enable_damage(),
            #[cfg(feature = "linux-displays")]
            Self::Selected(surface) => surface.enable_damage(),
        }
    }
    fn damage_unchanged(&mut self) -> Result<bool, NativeError> {
        match self {
            Self::Root(surface) => surface.damage_unchanged(),
            #[cfg(feature = "linux-displays")]
            Self::Selected(surface) => surface.damage_unchanged(),
        }
    }

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
    last_readback: Option<Instant>,
    last_readback_micros: Option<u64>,
    stats: CaptureStats,
}
impl ChangeAwareCapture {
    /// Counts actual X11 transfers, not source freshness or estimated GPU work.
    pub fn transfer_statistics(&self) -> Result<crate::image_transfer::Statistics, NativeError> {
        match &self.surface {
            CaptureSurface::Root(surface) => surface.transfer_statistics(),
            #[cfg(feature = "linux-displays")]
            CaptureSurface::Selected(surface) => surface.transfer_statistics(),
        }
    }

    /// Separate cursor observation: no capture freshness, frame ID or encode.
    pub fn capture_cursor(&mut self) -> Result<Option<crate::cursor::CursorSnapshot>, NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        match &mut self.surface {
            CaptureSurface::Root(surface) => surface.capture_cursor(),
            #[cfg(feature = "linux-displays")]
            CaptureSurface::Selected(surface) => surface.capture_cursor(),
        }
    }
    pub const fn new(surface: X11Surface, codec: HevcEncoder) -> Self {
        Self {
            surface: CaptureSurface::Root(surface),
            codec,
            baseline: None,
            pending: None,
            last_request: None,
            last_observed: None,
            closed: false,
            last_readback: None,
            last_readback_micros: None,
            stats: CaptureStats {
                readbacks: 0,
                damage_observations: 0,
                encoded_submissions: 0,
            },
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
            last_readback: None,
            last_readback_micros: None,
            stats: CaptureStats {
                readbacks: 0,
                damage_observations: 0,
                encoded_submissions: 0,
            },
        }
    }
    /// Enable DAMAGE on the original X capture connection. `false` means the
    /// server lacks this optimization; the exact full-readback path remains.
    /// This is X-drawable change evidence, never client visibility or authority.
    pub fn enable_damage_tracking(&mut self) -> Result<bool, NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        if self.pending.is_some() {
            return Err(NativeError::NeedDrain);
        }
        self.surface
            .enable_damage()
            .inspect_err(|_| self.closed = true)
    }
    pub const fn stats(&self) -> CaptureStats {
        self.stats
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
        // Source observations are scoped to the X drawable. Independently bound
        // both real worker time and parent clock distance between full checks;
        // duplicate/delayed parent timestamps cannot suppress readbacks forever.
        if only_if_changed
            && !force_idr
            && let Some(old) = &self.baseline
            && self
                .last_readback
                .is_some_and(|time| time.elapsed() < VERIFICATION_INTERVAL)
            && self
                .last_readback_micros
                .is_some_and(|time| observed_micros - time < 250_000)
            && self
                .surface
                .damage_unchanged()
                .inspect_err(|_| self.closed = true)?
        {
            self.stats.damage_observations = self.stats.damage_observations.saturating_add(1);
            return Ok(CaptureOutput::Unchanged(UnchangedCapture {
                candidate: frame,
                reference: old.frame,
                observed_micros,
            }));
        }
        // Nothing acquired below can bless the baseline until capture succeeds.
        let pixels = self
            .surface
            .snapshot()
            .inspect_err(|_| self.closed = true)?;
        self.last_readback = Some(Instant::now());
        self.last_readback_micros = Some(observed_micros);
        self.stats.readbacks = self.stats.readbacks.saturating_add(1);
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
        self.stats.encoded_submissions = self.stats.encoded_submissions.saturating_add(1);
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
