//! A scriptable in-memory codec for tests (feature `testing` only).
//!
//! This is a **test double, never a codec**: it produces byte-tagged
//! placeholder access units, not HEVC. It exists so that the contract's
//! ownership, backpressure, IDR-first, reconfiguration, wrong-backend, and
//! device-loss behaviour can be driven deterministically without hardware,
//! by this crate's tests and by downstream crates that need a media backend
//! in their own tests. It is gated behind the `testing` feature so it can
//! never be linked into a production binary.

use crate::access_unit::{EncodedAccessUnit, FrameId, FrameKind};
use crate::codec::{DecodedPicture, Decoder, EncodeRequest, Encoder, MediaError};
use crate::config::CodecConfiguration;
use crate::surface::{GpuSurface, PixelFormat, SurfaceBackend};
use fr_core::ids::RecoveryGeneration;
use fr_core::limits::ProtocolLimits;

/// An in-memory surface for tests.
#[derive(Debug, Clone)]
pub struct FakeSurface {
    format: PixelFormat,
    width: u32,
    height: u32,
    backend: SurfaceBackend,
}

impl FakeSurface {
    /// A fake capture surface with the given format and dimensions.
    #[must_use]
    pub fn new(format: PixelFormat, width: u32, height: u32) -> Self {
        Self {
            format,
            width,
            height,
            backend: SurfaceBackend::Fake,
        }
    }

    /// A surface tagged with a *different* backend, to exercise the
    /// wrong-backend rejection path.
    #[must_use]
    pub fn with_backend(mut self, backend: SurfaceBackend) -> Self {
        self.backend = backend;
        self
    }
}

impl GpuSurface for FakeSurface {
    fn backend(&self) -> SurfaceBackend {
        self.backend
    }
    fn format(&self) -> PixelFormat {
        self.format
    }
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
}

/// A scriptable fake encoder.
#[derive(Debug)]
pub struct FakeEncoder {
    limits: ProtocolLimits,
    config: Option<CodecConfiguration>,
    pending: Vec<EncodedAccessUnit>,
    next_frame: FrameId,
    last_frame: Option<FrameId>,
    idr_required: bool,
    recovery: RecoveryGeneration,
    in_flight: u32,
    backpressure_after: u32,
    device_lost_at_submit: Option<u32>,
    submit_count: u32,
}

impl FakeEncoder {
    /// A fake encoder that signals backpressure once `backpressure_after`
    /// submits are in flight without a drain.
    #[must_use]
    pub fn new(limits: ProtocolLimits, backpressure_after: u32) -> Self {
        Self {
            limits,
            config: None,
            pending: Vec::new(),
            next_frame: FrameId::FIRST,
            last_frame: None,
            idr_required: false,
            recovery: RecoveryGeneration::INITIAL,
            in_flight: 0,
            backpressure_after: backpressure_after.max(1),
            device_lost_at_submit: None,
            submit_count: 0,
        }
    }

    /// Scripts a device-lost error on the Nth submit (1-based).
    #[must_use]
    pub fn lose_device_at_submit(mut self, n: u32) -> Self {
        self.device_lost_at_submit = Some(n);
        self
    }
}

impl Encoder for FakeEncoder {
    fn configure(&mut self, config: CodecConfiguration) -> Result<(), MediaError> {
        self.config = Some(config);
        // A (re)configuration forces the next output to be an IDR.
        self.idr_required = true;
        self.last_frame = None;
        Ok(())
    }

    fn submit(
        &mut self,
        surface: &dyn GpuSurface,
        request: EncodeRequest,
    ) -> Result<(), MediaError> {
        let config = self.config.ok_or(MediaError::NotConfigured)?;
        self.submit_count += 1;
        if Some(self.submit_count) == self.device_lost_at_submit {
            return Err(MediaError::DeviceLost);
        }
        if surface.backend() != SurfaceBackend::Fake {
            return Err(MediaError::WrongBackend {
                expected: SurfaceBackend::Fake,
                found: surface.backend(),
            });
        }
        if self.in_flight >= self.backpressure_after {
            // Undo the submit_count bump on backpressure: the caller retries
            // the same surface after draining.
            self.submit_count -= 1;
            return Err(MediaError::Backpressure);
        }

        let make_idr = self.idr_required || request.force_idr;
        let (kind, this_frame) = if make_idr {
            let recovery = self.recovery;
            self.recovery = recovery.next().unwrap_or(RecoveryGeneration::INITIAL);
            self.idr_required = false;
            (FrameKind::Idr { recovery }, self.next_frame)
        } else {
            let references = self.last_frame.expect("non-IDR requires a prior reference");
            (FrameKind::Predicted { references }, self.next_frame)
        };

        // Placeholder bytes tag the frame number; this is not HEVC.
        let bytes = this_frame.as_raw().to_be_bytes().to_vec();
        let au = EncodedAccessUnit::new(
            &self.limits,
            this_frame,
            kind,
            config.generation(),
            u64::from(surface.width()),
            bytes,
        )
        .map_err(|_| MediaError::Fatal)?;

        self.pending.push(au);
        self.last_frame = Some(this_frame);
        self.next_frame = self.next_frame.next().ok_or(MediaError::Fatal)?;
        self.in_flight += 1;
        Ok(())
    }

    fn poll_output(&mut self) -> Result<EncodedAccessUnit, MediaError> {
        if self.pending.is_empty() {
            return Err(MediaError::NeedMoreInput);
        }
        self.in_flight = self.in_flight.saturating_sub(1);
        Ok(self.pending.remove(0))
    }

    fn configuration(&self) -> Option<CodecConfiguration> {
        self.config
    }
}

/// A decoded picture produced by [`FakeDecoder`], owning its surface.
#[derive(Debug)]
pub struct FakeDecodedPicture {
    frame: u64,
    surface: FakeSurface,
}

impl DecodedPicture for FakeDecodedPicture {
    fn frame_raw(&self) -> u64 {
        self.frame
    }
    fn surface(&self) -> &dyn GpuSurface {
        &self.surface
    }
}

/// A scriptable fake decoder that mirrors the encoder's contract.
#[derive(Debug)]
pub struct FakeDecoder {
    config: Option<CodecConfiguration>,
    pending: Vec<u64>,
    idr_required: bool,
}

impl FakeDecoder {
    /// A fresh fake decoder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: None,
            pending: Vec::new(),
            idr_required: false,
        }
    }
}

impl Default for FakeDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder for FakeDecoder {
    fn configure(&mut self, config: CodecConfiguration) -> Result<(), MediaError> {
        self.config = Some(config);
        self.idr_required = true;
        Ok(())
    }

    fn submit(&mut self, access_unit: &EncodedAccessUnit) -> Result<(), MediaError> {
        let config = self.config.ok_or(MediaError::NotConfigured)?;
        if access_unit.config_generation() != config.generation() {
            return Err(MediaError::ConfigMismatch);
        }
        if self.idr_required && !access_unit.is_idr() {
            // A freshly configured/reset decoder cannot accept a continuation
            // as if reference state were intact (plan section 16.3).
            return Err(MediaError::ConfigMismatch);
        }
        if access_unit.is_idr() {
            self.idr_required = false;
        }
        self.pending.push(access_unit.frame().as_raw());
        Ok(())
    }

    fn poll_output(&mut self) -> Result<Box<dyn DecodedPicture + '_>, MediaError> {
        if self.pending.is_empty() {
            return Err(MediaError::NeedMoreInput);
        }
        let frame = self.pending.remove(0);
        let surface = FakeSurface::new(PixelFormat::Nv12, 1920, 1080);
        Ok(Box::new(FakeDecodedPicture { frame, surface }))
    }

    fn configuration(&self) -> Option<CodecConfiguration> {
        self.config
    }
}
