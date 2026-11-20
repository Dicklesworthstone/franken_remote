//! Published codec state of one `VideoToolbox` owner. Native property readback
//! alone cannot advance it: only an admitted output confirms an encoder rate.
use fr_media::access_unit::FrameId;

pub(super) struct Output {
    reference: Option<FrameId>,
    bitrate: u32,
    installed: u32,
    pending_idr: bool,
}

impl Output {
    pub(super) const fn new(bitrate: u32) -> Self {
        Self {
            reference: None,
            bitrate,
            installed: bitrate,
            pending_idr: false,
        }
    }

    pub(super) const fn reference(&self) -> Option<FrameId> {
        self.reference
    }

    pub(super) const fn bitrate(&self) -> u32 {
        self.bitrate
    }

    pub(super) const fn force_idr(&self, requested: bool) -> bool {
        requested || self.reference.is_none() || self.pending_idr
    }

    /// A positively completed native drop owns no output or freshness. Keep
    /// the reference and advertised rate, including across target cancellation.
    pub(super) fn skipped(
        &mut self,
        target: u32,
        actual: u32,
        length: usize,
        idr: bool,
        forced: bool,
    ) -> Result<(), ()> {
        if self.reference.is_none()
            || length != 0
            || idr
            || (actual != self.installed && actual != target)
        {
            return Err(());
        }
        self.pending_idr |= forced || actual != self.installed;
        self.installed = actual;
        Ok(())
    }

    /// Called only after HEVC admission and the dual-clock rate confirmation.
    pub(super) fn encoded(&mut self, frame: FrameId, actual: u32) {
        self.reference = Some(frame);
        self.bitrate = actual;
        self.installed = actual;
        self.pending_idr = false;
    }
}
