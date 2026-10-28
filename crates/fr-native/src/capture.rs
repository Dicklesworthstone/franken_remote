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

mod refinement;
use refinement::{Moment, Refinement};
mod rate;
use rate::Rate;

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
    /// Successfully collected optional refinements, included in submissions.
    pub refinements: u64,
    /// Latest locally requested target, which may await damage or the rate gate.
    pub requested_bitrate_bps: u32,
    /// Last encoder rate with collected, validated output from the same source.
    pub encoder_bitrate_bps: u32,
    pub bitrate_reconfigurations: u64,
}
const VERIFICATION_INTERVAL: Duration = Duration::from_millis(250);
struct Snapshot {
    frame: FrameId,
    pixels: BgraFrame,
    refined: bool,
    bitrate: Option<u32>,
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
    refinement: Option<Refinement>,
    rate: Rate,
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
        let bitrate = codec.bitrate();
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
                refinements: 0,
                requested_bitrate_bps: bitrate,
                encoder_bitrate_bps: bitrate,
                bitrate_reconfigurations: 0,
            },
            refinement: None,
            rate: Rate::new(bitrate),
        }
    }
    #[cfg(feature = "linux-displays")]
    pub const fn selected(
        surface: crate::displays::X11SelectedCapture,
        codec: HevcEncoder,
    ) -> Self {
        let bitrate = codec.bitrate();
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
                refinements: 0,
                requested_bitrate_bps: bitrate,
                encoder_bitrate_bps: bitrate,
                bitrate_reconfigurations: 0,
            },
            refinement: None,
            rate: Rate::new(bitrate),
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
    /// Enable one bounded, newly verified settle refinement per real damage
    /// burst before capture starts. Only the measured libx265 path implements
    /// the per-picture quantization control; hardware returns `false`.
    pub fn enable_settle_refinement(&mut self) -> Result<bool, NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        if self.last_request.is_some() || self.refinement.is_some() {
            return Err(NativeError::InvalidConfiguration);
        }
        if !self.codec.supports_refinement() {
            return Ok(false);
        }
        self.refinement = Some(Refinement::default());
        Ok(true)
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
        self.capture_request(frame, observed_micros, force_idr, only_if_changed, None)
    }
    /// Request a bounded software bitrate on the original source. Static
    /// observations do not restart the encoder or manufacture video. A target
    /// waits for actual changed/unconditional/recovery capture and the native
    /// dual-clock dwell; a settle-only refinement does not trigger the switch.
    /// Hardware refuses before any native mutation. Every applied switch is a
    /// verified IDR with the original exact decoder configuration and timestamp.
    pub fn capture_with_bitrate(
        &mut self,
        frame: FrameId,
        observed_micros: u64,
        force_idr: bool,
        only_if_changed: bool,
        bitrate: u32,
    ) -> Result<CaptureOutput, NativeError> {
        self.capture_request(
            frame,
            observed_micros,
            force_idr,
            only_if_changed,
            Some(bitrate),
        )
    }
    fn capture_request(
        &mut self,
        frame: FrameId,
        observed_micros: u64,
        force_idr: bool,
        only_if_changed: bool,
        bitrate: Option<u32>,
    ) -> Result<CaptureOutput, NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        if self.pending.is_some() {
            return Err(NativeError::NeedDrain);
        }
        if let Some(bitrate) = bitrate {
            self.codec.validate_bitrate(bitrate)?;
        }
        if self.last_request.is_some_and(|id| frame <= id)
            || self.last_observed.is_some_and(|t| observed_micros < t)
        {
            self.closed = true;
            return Err(NativeError::StaleGeneration);
        }
        self.last_request = Some(frame);
        self.last_observed = Some(observed_micros);
        // Legacy capture is an explicit cancellation of deferred rate intent.
        // Recovery owners using the old IPC cannot apply an unacknowledged rate
        // change. Keep this AFTER every pending/identity/clock preflight.
        let requested = bitrate.unwrap_or(self.codec.bitrate());
        self.rate.request(requested);
        self.stats.requested_bitrate_bps = requested;
        let observation = Moment::new(observed_micros, Instant::now());
        let refinement_due = self.codec.refinement_ready()
            && self
                .refinement
                .as_ref()
                .is_some_and(|state| state.due(observation));
        // Source observations are scoped to the X drawable. Independently bound
        // both real worker time and parent clock distance between full checks;
        // duplicate/delayed parent timestamps cannot suppress readbacks forever.
        if only_if_changed
            && !force_idr
            && !refinement_due
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
            if let Some(state) = &mut self.refinement {
                state.unchanged(observation);
            }
            return Ok(CaptureOutput::Unchanged(UnchangedCapture {
                candidate: frame,
                reference: old.frame,
                observed_micros,
            }));
        }
        self.capture_pixels(
            frame,
            observed_micros,
            force_idr,
            only_if_changed,
            observation,
        )
    }
    fn capture_pixels(
        &mut self,
        frame: FrameId,
        observed_micros: u64,
        force_idr: bool,
        only_if_changed: bool,
        observation: Moment,
    ) -> Result<CaptureOutput, NativeError> {
        // Nothing acquired below can bless the baseline until capture succeeds.
        let pixels = self
            .surface
            .snapshot()
            .inspect_err(|_| self.closed = true)?;
        self.last_readback = Some(Instant::now());
        self.last_readback_micros = Some(observed_micros);
        self.stats.readbacks = self.stats.readbacks.saturating_add(1);
        let unchanged = self.baseline.as_ref().is_some_and(|old| {
            old.pixels.width() == pixels.width()
                && old.pixels.height() == pixels.height()
                && old.pixels.pixels() == pixels.pixels()
        });
        if unchanged && let Some(state) = &mut self.refinement {
            state.unchanged(observation);
        }
        let refine = only_if_changed
            && !force_idr
            && unchanged
            && self.codec.refinement_ready()
            && self
                .refinement
                .as_ref()
                .is_some_and(|state| state.due(observation));
        if only_if_changed && !force_idr && unchanged && !refine {
            self.surface.verify().inspect_err(|_| self.closed = true)?;
            let old = self.baseline.as_ref().expect("compared baseline");
            return Ok(CaptureOutput::Unchanged(UnchangedCapture {
                candidate: frame,
                reference: old.frame,
                observed_micros,
            }));
        }
        // The final comparison gives real changed pixels priority, even when a
        // refinement was due before this full readback. No queued old snapshot
        // is re-encoded and no extra capture/encoder queue is introduced.
        let bitrate = if refine {
            None
        } else {
            self.rate
                .due(observed_micros, Instant::now(), self.baseline.is_some())
        };
        let applied = if let Some(bitrate) = bitrate {
            self.codec
                .reopen_bitrate(bitrate)
                .inspect_err(|_| self.closed = true)?
                .then_some(bitrate)
        } else {
            None
        };
        let submitted = if refine {
            self.codec
                .submit_refinement(&pixels, frame, observed_micros)
        } else {
            self.codec
                .submit(&pixels, frame, observed_micros, force_idr)
        };
        submitted.inspect_err(|_| self.closed = true)?;
        if let Some(state) = &mut self.refinement {
            if refine {
                state.admitted(observation);
            } else if !unchanged {
                state.changed();
            }
        }
        self.stats.encoded_submissions = self.stats.encoded_submissions.saturating_add(1);
        self.pending = Some(Snapshot {
            frame,
            pixels,
            refined: refine,
            bitrate: applied,
        });
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
        if self.pending.as_ref().is_some_and(|pending| pending.refined) {
            self.stats.refinements = self.stats.refinements.saturating_add(1);
        }
        if let Some(bitrate) = self.pending.as_ref().and_then(|pending| pending.bitrate) {
            if !unit.is_idr() {
                self.closed = true;
                return Err(NativeError::UnsupportedBitstream);
            }
            self.rate
                .applied(bitrate, unit.capture_micros(), Instant::now());
            self.stats.encoder_bitrate_bps = bitrate;
            self.stats.bitrate_reconfigurations =
                self.stats.bitrate_reconfigurations.saturating_add(1);
        }
        // Move the snapshot only AFTER the validated access unit exists. Pending
        // or refused native submissions cannot become an unchanged-source anchor.
        self.baseline = self.pending.take();
        Ok(unit)
    }
}
