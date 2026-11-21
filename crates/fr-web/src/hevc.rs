//! Browser HEVC WebCodecs pipeline guard and non-circular startup state machine.
//!
//! Enforces:
//! - Exact negotiated `hvcC` and SPS-derived `hvc1.*` codec string via [`HevcGuard`].
//! - Non-circular startup stages (`AwaitingConfig` -> `Configured` ->
//!   `FirstFrameDecoded` -> `Presented`), each distinct with explicit transitions.
//! - Rejection of unannounced parameter-set changes and delta frames before an IDR.
//! - Bounded in-flight decode + renderer frame retention: credit is released ONLY
//!   when the browser confirms `VideoFrame.close()` for a tracked frame ID.

use core::fmt;
use fr_core::{ids::CodecConfigurationGeneration, limits::ProtocolLimits};
use fr_media::{
    config::CodecConfiguration,
    hevc::{HevcError, HevcGuard},
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Maximum in-flight WebCodecs decode + renderer frames retained simultaneously.
pub const MAX_BROWSER_INFLIGHT_FRAMES: usize = 4;

/// Non-circular startup stages for browser HEVC playback (`AGENTS.md` §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BrowserDecoderStage {
    /// No `DecoderConfiguration` admitted yet.
    AwaitingConfig,
    /// `VideoDecoder.configure()` succeeded with validated `hvcC`; awaiting verified IDR.
    Configured { generation: u64 },
    /// First verified IDR decoded by `VideoDecoder`; not yet visibly presented.
    FirstFrameDecoded { generation: u64, frame_id: u64 },
    /// At least one frame visibly presented on a non-hidden canvas.
    Presented {
        generation: u64,
        last_frame_id: u64,
        last_source_us: u64,
        last_presented_us: u64,
    },
    /// Decoder failed, was reclaimed by browser, or reference chain broke; recovery IDR required.
    NeedsRecovery { generation: u64 },
}

/// Validated configuration payload handed to JS `VideoDecoder.configure()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebCodecsConfig {
    pub generation: CodecConfigurationGeneration,
    pub codec_string: String,
    pub hvcc: Vec<u8>,
    pub coded_width: u32,
    pub coded_height: u32,
    pub crop_width: u32,
    pub crop_height: u32,
}

/// One validated `hvc1` access unit ready for JS `EncodedVideoChunk` submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedWebCodecsChunk {
    pub generation: CodecConfigurationGeneration,
    pub frame_id: u64,
    pub is_key: bool,
    pub source_timestamp_us: u64,
    pub charged_bytes: u64,
    pub hvc1_bytes: Vec<u8>,
}

/// Resource release proof emitted ONLY when `VideoFrame.close()` completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedFrameRelease {
    pub frame_id: u64,
    pub freed_bytes: u64,
    pub remaining_inflight_frames: usize,
    pub remaining_inflight_bytes: u64,
}

/// Typed errors from the browser HEVC admission and tracking pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserHevcError {
    NotConfigured,
    StaleGeneration,
    AwaitingIdr,
    DecodeQueueFull,
    UnknownFrameId,
    HiddenPresentationForbidden,
    Hevc(HevcError),
}

impl fmt::Display for BrowserHevcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfigured => f.write_str("browser HEVC decoder is not configured"),
            Self::StaleGeneration => f.write_str("stale codec configuration generation rejected"),
            Self::AwaitingIdr => f.write_str("delta frame rejected before verified IDR"),
            Self::DecodeQueueFull => f.write_str("browser decode queue ceiling reached"),
            Self::UnknownFrameId => f.write_str("unknown or already-closed VideoFrame ID"),
            Self::HiddenPresentationForbidden => {
                f.write_str("hidden or background tab cannot claim visible presentation")
            }
            Self::Hevc(e) => write!(f, "HEVC guard refused bitstream: {e:?}"),
        }
    }
}

impl core::error::Error for BrowserHevcError {}

#[derive(Debug, Clone, Copy)]
struct InFlightFrame {
    frame_id: u64,
    source_timestamp_us: u64,
    charged_bytes: u64,
}

/// Bounded browser HEVC admission, startup-stage, and `VideoFrame` lifetime tracker.
#[derive(Debug)]
pub struct BrowserHevcPipeline {
    guard: Option<HevcGuard>,
    active_generation: Option<CodecConfigurationGeneration>,
    stage: BrowserDecoderStage,
    awaiting_idr: bool,
    max_inflight_frames: usize,
    max_inflight_bytes: u64,
    inflight: VecDeque<InFlightFrame>,
    inflight_bytes: u64,
}

impl BrowserHevcPipeline {
    /// Construct a new unconfigured browser HEVC pipeline with bounded retention.
    #[must_use]
    pub fn new(max_inflight_frames: usize, max_inflight_bytes: u64) -> Self {
        let cap = max_inflight_frames.clamp(1, MAX_BROWSER_INFLIGHT_FRAMES);
        Self {
            guard: None,
            active_generation: None,
            stage: BrowserDecoderStage::AwaitingConfig,
            awaiting_idr: true,
            max_inflight_frames: cap,
            max_inflight_bytes: max_inflight_bytes.max(1024),
            inflight: VecDeque::with_capacity(cap),
            inflight_bytes: 0,
        }
    }

    /// Current startup / operational stage.
    #[must_use]
    pub const fn stage(&self) -> BrowserDecoderStage {
        self.stage
    }

    /// Number of currently unclosed frames in decode or renderer retention.
    #[must_use]
    pub fn inflight_frames(&self) -> usize {
        self.inflight.len()
    }

    /// Total charged bytes currently held in decode or renderer retention.
    #[must_use]
    pub const fn inflight_bytes(&self) -> u64 {
        self.inflight_bytes
    }

    /// Validate a host `DecoderConfiguration` `hvcC` payload and prepare the
    /// exact `WebCodecsConfig` (deriving `hvc1.*` from the actual SPS, never hard-coded).
    pub fn admit_decoder_configuration(
        &mut self,
        config: CodecConfiguration,
        limits: ProtocolLimits,
        max_decoded_pictures: u8,
        hvcc_bytes: &[u8],
    ) -> Result<WebCodecsConfig, BrowserHevcError> {
        let guard =
            HevcGuard::from_decoder_record(config, limits, max_decoded_pictures, hvcc_bytes)
                .map_err(BrowserHevcError::Hevc)?;
        let record = guard.decoder_record().map_err(BrowserHevcError::Hevc)?;
        let geom = config.geometry();
        let generation = config.generation();

        // Configuration replaces the old codec generation; clear old unclosed tracking
        // and require a verified IDR before any delta frame.
        self.guard = Some(guard);
        self.active_generation = Some(generation);
        self.awaiting_idr = true;
        self.inflight.clear();
        self.inflight_bytes = 0;
        self.stage = BrowserDecoderStage::AwaitingConfig;

        Ok(WebCodecsConfig {
            generation,
            codec_string: record.codec().to_owned(),
            hvcc: record.bytes().to_vec(),
            coded_width: geom.coded_width(),
            coded_height: geom.coded_height(),
            crop_width: geom.crop_width(),
            crop_height: geom.crop_height(),
        })
    }

    /// Record that `VideoDecoder.configure()` succeeded in the browser (distinct from first frame!).
    pub fn on_decoder_configured(
        &mut self,
        generation: CodecConfigurationGeneration,
    ) -> Result<(), BrowserHevcError> {
        if self.active_generation != Some(generation) || self.guard.is_none() {
            return Err(BrowserHevcError::StaleGeneration);
        }
        self.stage = BrowserDecoderStage::Configured {
            generation: generation.as_raw(),
        };
        self.awaiting_idr = true;
        Ok(())
    }

    /// Validate one length-prefixed wire HEVC access unit through [`HevcGuard::prepare_hvc1`],
    /// enforce bounded decode queue limits, and return the `hvc1` bytes for `EncodedVideoChunk`.
    pub fn admit_access_unit(
        &mut self,
        generation: CodecConfigurationGeneration,
        frame_id: u64,
        source_timestamp_us: u64,
        declared_idr: bool,
        length_prefixed_au: &[u8],
    ) -> Result<PreparedWebCodecsChunk, BrowserHevcError> {
        if self.active_generation != Some(generation) {
            return Err(BrowserHevcError::StaleGeneration);
        }
        if matches!(
            self.stage,
            BrowserDecoderStage::AwaitingConfig | BrowserDecoderStage::NeedsRecovery { .. }
        ) {
            return Err(BrowserHevcError::NotConfigured);
        }
        if self.awaiting_idr && !declared_idr {
            return Err(BrowserHevcError::AwaitingIdr);
        }
        let charged_bytes = u64::try_from(length_prefixed_au.len())
            .map_err(|_| BrowserHevcError::Hevc(HevcError::Limit))?;
        if charged_bytes == 0 {
            return Err(BrowserHevcError::Hevc(HevcError::Truncated));
        }
        if self.inflight.len() >= self.max_inflight_frames
            || self.inflight_bytes.saturating_add(charged_bytes) > self.max_inflight_bytes
        {
            return Err(BrowserHevcError::DecodeQueueFull);
        }

        let guard = self.guard.as_mut().ok_or(BrowserHevcError::NotConfigured)?;
        let prepared = guard
            .prepare_hvc1(length_prefixed_au, declared_idr)
            .map_err(BrowserHevcError::Hevc)?;

        if prepared.picture().idr {
            self.awaiting_idr = false;
        }

        self.inflight.push_back(InFlightFrame {
            frame_id,
            source_timestamp_us,
            charged_bytes,
        });
        self.inflight_bytes = self.inflight_bytes.saturating_add(charged_bytes);

        Ok(PreparedWebCodecsChunk {
            generation,
            frame_id,
            is_key: prepared.picture().idr,
            source_timestamp_us,
            charged_bytes,
            hvc1_bytes: prepared.bytes().to_vec(),
        })
    }

    /// Record that `VideoDecoder` output a decoded frame (does NOT release credit yet!).
    pub fn on_frame_decoded(
        &mut self,
        generation: CodecConfigurationGeneration,
        frame_id: u64,
    ) -> Result<(), BrowserHevcError> {
        if self.active_generation != Some(generation) {
            return Err(BrowserHevcError::StaleGeneration);
        }
        if !self.inflight.iter().any(|f| f.frame_id == frame_id) {
            return Err(BrowserHevcError::UnknownFrameId);
        }
        if matches!(self.stage, BrowserDecoderStage::Configured { .. }) {
            self.stage = BrowserDecoderStage::FirstFrameDecoded {
                generation: generation.as_raw(),
                frame_id,
            };
        }
        Ok(())
    }

    /// Record visible presentation on a non-hidden canvas (does NOT release credit until close!).
    pub fn on_frame_presented(
        &mut self,
        generation: CodecConfigurationGeneration,
        frame_id: u64,
        now_us: u64,
        visible: bool,
    ) -> Result<(), BrowserHevcError> {
        if !visible {
            return Err(BrowserHevcError::HiddenPresentationForbidden);
        }
        if self.active_generation != Some(generation) {
            return Err(BrowserHevcError::StaleGeneration);
        }
        let Some(entry) = self.inflight.iter().find(|f| f.frame_id == frame_id) else {
            return Err(BrowserHevcError::UnknownFrameId);
        };
        self.stage = BrowserDecoderStage::Presented {
            generation: generation.as_raw(),
            last_frame_id: frame_id,
            last_source_us: entry.source_timestamp_us,
            last_presented_us: now_us,
        };
        Ok(())
    }

    /// Record that `VideoFrame.close()` freed the frame, returning the freed byte count
    /// so the caller can advance `0x0081 ReceiverPressure` cumulative credit.
    pub fn on_video_frame_closed(
        &mut self,
        frame_id: u64,
    ) -> Result<DecodedFrameRelease, BrowserHevcError> {
        let pos = self
            .inflight
            .iter()
            .position(|f| f.frame_id == frame_id)
            .ok_or(BrowserHevcError::UnknownFrameId)?;
        let entry = self
            .inflight
            .remove(pos)
            .ok_or(BrowserHevcError::UnknownFrameId)?;
        self.inflight_bytes = self.inflight_bytes.saturating_sub(entry.charged_bytes);
        Ok(DecodedFrameRelease {
            frame_id: entry.frame_id,
            freed_bytes: entry.charged_bytes,
            remaining_inflight_frames: self.inflight.len(),
            remaining_inflight_bytes: self.inflight_bytes,
        })
    }

    /// Fence the decoder after WebCodecs reclamation or decode error; requires fresh recovery IDR.
    pub fn on_decoder_reclaimed(&mut self) {
        let generation = self
            .active_generation
            .map_or(0, CodecConfigurationGeneration::as_raw);
        self.awaiting_idr = true;
        self.stage = BrowserDecoderStage::NeedsRecovery { generation };
    }
}
