//! Browser HEVC `WebCodecs` pipeline guard and non-circular startup state machine.
//!
//! Enforces:
//! - Exact negotiated `hvcC` and SPS-derived `hvc1.*` codec string via [`HevcGuard`].
//! - Non-circular startup stages (`AwaitingConfig` -> `Configured` ->
//!   `FirstFrameDecoded` -> `Presented`), each distinct with explicit transitions.
//! - Rejection of unannounced parameter-set changes and delta frames before an IDR.
//! - Bounded in-flight decode + renderer frame retention: credit is released ONLY
//!   when the browser confirms `VideoFrame.close()` for a tracked frame ID.

use crate::credit::BrowserCreditError;
use core::fmt;
use fr_core::{ids::CodecConfigurationGeneration, limits::ProtocolLimits};
use fr_media::{
    config::CodecConfiguration,
    hevc::{HevcError, HevcGuard},
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Maximum in-flight `WebCodecs` decode + renderer frames retained simultaneously.
pub const MAX_BROWSER_INFLIGHT_FRAMES: usize = 4;
/// Bound decoder objects across reconfiguration and delayed shutdown callbacks.
pub const MAX_BROWSER_DECODERS: usize = 4;

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
    pub decoder: DecoderInstance,
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
    pub decoder: DecoderInstance,
    pub generation: CodecConfigurationGeneration,
    pub frame_id: u64,
    pub is_key: bool,
    pub source_timestamp_us: u64,
    pub charged_bytes: u64,
    pub hvc1_bytes: Vec<u8>,
}

/// Original resource release after output closure or completed decoder/input disposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedFrameRelease {
    pub decoder: DecoderInstance,
    pub frame_id: u64,
    pub freed_bytes: u64,
    pub remaining_inflight_frames: usize,
    pub remaining_inflight_bytes: u64,
}

/// Original local decoder owner captured by every browser callback.
///
/// A new decoder after recovery can use the same wire codec configuration. Its
/// local serial still advances, so queued callbacks from the previous decoder
/// cannot describe the replacement. This token grants no session authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DecoderInstance {
    pub generation: CodecConfigurationGeneration,
    pub serial: u64,
}

/// Typed errors from the browser HEVC admission and tracking pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserHevcError {
    NotConfigured,
    StaleGeneration,
    GenerationExhausted,
    InvalidDecoderState,
    InvalidFrameOrder,
    AlreadyDecoded,
    FrameNotDecoded,
    InvalidPresentationOrder,
    DecoderCapacity,
    AwaitingIdr,
    DecodeQueueFull,
    UnknownFrameId,
    HiddenPresentationForbidden,
    Credit(BrowserCreditError),
    Hevc(HevcError),
}

impl fmt::Display for BrowserHevcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfigured => f.write_str("browser HEVC decoder is not configured"),
            Self::StaleGeneration => f.write_str("stale codec configuration generation rejected"),
            Self::GenerationExhausted => f.write_str("browser decoder identity exhausted"),
            Self::InvalidDecoderState => f.write_str("browser decoder transition refused"),
            Self::InvalidFrameOrder => f.write_str("reused or out-of-order browser frame ID"),
            Self::AlreadyDecoded => f.write_str("browser frame already decoded"),
            Self::FrameNotDecoded => f.write_str("browser frame has not been decoded"),
            Self::InvalidPresentationOrder => {
                f.write_str("repeated or out-of-order browser presentation")
            }
            Self::DecoderCapacity => f.write_str("browser decoder retirement capacity reached"),
            Self::AwaitingIdr => f.write_str("delta frame rejected before verified IDR"),
            Self::DecodeQueueFull => f.write_str("browser decode queue ceiling reached"),
            Self::UnknownFrameId => f.write_str("unknown or already-closed VideoFrame ID"),
            Self::HiddenPresentationForbidden => {
                f.write_str("hidden or background tab cannot claim visible presentation")
            }
            Self::Credit(e) => write!(f, "browser receive credit refused: {e}"),
            Self::Hevc(e) => write!(f, "HEVC guard refused bitstream: {e:?}"),
        }
    }
}

impl core::error::Error for BrowserHevcError {}

#[derive(Debug, Clone, Copy)]
struct InFlightFrame {
    decoder: DecoderInstance,
    frame_id: u64,
    source_timestamp_us: u64,
    charged_bytes: u64,
    decoded: bool,
    escaped: bool,
    idr: bool,
}

#[derive(Debug, Clone, Copy)]
struct DecoderOwner {
    instance: DecoderInstance,
    closed: bool,
}

/// Bounded browser HEVC admission, startup-stage, and `VideoFrame` lifetime tracker.
#[derive(Debug)]
pub struct BrowserHevcPipeline {
    guard: Option<HevcGuard>,
    configuration: Option<CodecConfiguration>,
    active_decoder: Option<DecoderInstance>,
    last_decoder_serial: u64,
    decoders: Vec<DecoderOwner>,
    stage: BrowserDecoderStage,
    awaiting_idr: bool,
    max_inflight_frames: usize,
    max_inflight_bytes: u64,
    inflight: VecDeque<InFlightFrame>,
    inflight_bytes: u64,
    last_admitted_frame: Option<u64>,
    last_presented_frame: Option<u64>,
}

impl BrowserHevcPipeline {
    /// Construct a new unconfigured browser HEVC pipeline with bounded retention.
    #[must_use]
    pub fn new(max_inflight_frames: usize, max_inflight_bytes: u64) -> Self {
        let cap = max_inflight_frames.min(MAX_BROWSER_INFLIGHT_FRAMES);
        Self {
            guard: None,
            configuration: None,
            active_decoder: None,
            last_decoder_serial: 0,
            decoders: Vec::with_capacity(MAX_BROWSER_DECODERS),
            stage: BrowserDecoderStage::AwaitingConfig,
            awaiting_idr: true,
            max_inflight_frames: cap,
            max_inflight_bytes,
            inflight: VecDeque::with_capacity(cap),
            inflight_bytes: 0,
            last_admitted_frame: None,
            last_presented_frame: None,
        }
    }

    /// Current local decoder identity, including while it is awaiting setup or recovery.
    #[must_use]
    pub const fn active_decoder(&self) -> Option<DecoderInstance> {
        self.active_decoder
    }

    /// Decoder objects still awaiting positive shutdown or escaped-frame release.
    #[must_use]
    pub fn retained_decoders(&self) -> usize {
        self.decoders.len()
    }

    /// Whether an original decoder still owns a native object or output charge.
    #[must_use]
    pub fn contains_decoder(&self, decoder: DecoderInstance) -> bool {
        self.decoders.iter().any(|owner| owner.instance == decoder)
    }

    /// Start another session only after every original native decoder and
    /// escaped output has closed. Keep local callback serials non-reusable even
    /// when the new host session starts again at wire codec generation zero.
    pub fn reset_session(&mut self) -> Result<(), BrowserHevcError> {
        if !self.decoders.is_empty() || !self.inflight.is_empty() {
            return Err(BrowserHevcError::InvalidDecoderState);
        }
        self.guard = None;
        self.configuration = None;
        self.active_decoder = None;
        self.stage = BrowserDecoderStage::AwaitingConfig;
        self.awaiting_idr = true;
        self.last_admitted_frame = None;
        self.last_presented_frame = None;
        Ok(())
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
        let generation = config.generation();
        if let Some(active) = self.active_decoder {
            if generation < active.generation {
                return Err(BrowserHevcError::StaleGeneration);
            }
            if generation == active.generation {
                if !matches!(self.stage, BrowserDecoderStage::NeedsRecovery { .. }) {
                    return Err(BrowserHevcError::InvalidDecoderState);
                }
                // A decoder reset changes its local owner, not the host's codec
                // configuration. Reusing that wire generation may not change
                // geometry, color, GOP policy or any frozen parameter set.
                if self.configuration != Some(config)
                    || self
                        .guard
                        .as_ref()
                        .ok_or(BrowserHevcError::NotConfigured)?
                        .decoder_record()
                        .map_err(BrowserHevcError::Hevc)?
                        .bytes()
                        != hvcc_bytes
                {
                    return Err(BrowserHevcError::Hevc(HevcError::ChangedParameterSet));
                }
            }
        }
        let serial = self
            .last_decoder_serial
            .checked_add(1)
            .ok_or(BrowserHevcError::GenerationExhausted)?;
        if self.decoders.len() >= MAX_BROWSER_DECODERS {
            return Err(BrowserHevcError::DecoderCapacity);
        }
        let guard =
            HevcGuard::from_decoder_record(config, limits, max_decoded_pictures, hvcc_bytes)
                .map_err(BrowserHevcError::Hevc)?;
        let record = guard.decoder_record().map_err(BrowserHevcError::Hevc)?;
        let geom = config.geometry();
        let decoder = DecoderInstance { generation, serial };

        // Retire callbacks from the old decoder immediately, but do not pretend
        // its queued native work or output surfaces were released. They continue
        // to count against the SAME queue and byte ceilings until actual cleanup.
        self.guard = Some(guard);
        self.configuration = Some(config);
        self.active_decoder = Some(decoder);
        self.last_decoder_serial = serial;
        self.decoders.push(DecoderOwner {
            instance: decoder,
            closed: false,
        });
        self.awaiting_idr = true;
        self.last_admitted_frame = None;
        self.last_presented_frame = None;
        self.stage = BrowserDecoderStage::AwaitingConfig;

        Ok(WebCodecsConfig {
            decoder,
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
        decoder: DecoderInstance,
    ) -> Result<(), BrowserHevcError> {
        if self.active_decoder != Some(decoder) || self.guard.is_none() {
            return Err(BrowserHevcError::StaleGeneration);
        }
        if self.stage != BrowserDecoderStage::AwaitingConfig {
            return Err(BrowserHevcError::InvalidDecoderState);
        }
        self.stage = BrowserDecoderStage::Configured {
            generation: decoder.generation.as_raw(),
        };
        self.awaiting_idr = true;
        Ok(())
    }

    /// Validate one length-prefixed wire HEVC access unit through [`HevcGuard::prepare_hvc1`],
    /// enforce bounded decode queue limits, and return the `hvc1` bytes for `EncodedVideoChunk`.
    pub fn admit_access_unit(
        &mut self,
        decoder: DecoderInstance,
        frame_id: u64,
        source_timestamp_us: u64,
        declared_idr: bool,
        length_prefixed_au: &[u8],
    ) -> Result<PreparedWebCodecsChunk, BrowserHevcError> {
        if self.active_decoder != Some(decoder) {
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
        if self
            .last_admitted_frame
            .is_some_and(|last| frame_id <= last)
        {
            return Err(BrowserHevcError::InvalidFrameOrder);
        }
        let charged_bytes = u64::try_from(length_prefixed_au.len())
            .map_err(|_| BrowserHevcError::Hevc(HevcError::Limit))?;
        if charged_bytes == 0 {
            return Err(BrowserHevcError::Hevc(HevcError::Truncated));
        }
        let next_bytes = self
            .inflight_bytes
            .checked_add(charged_bytes)
            .ok_or(BrowserHevcError::DecodeQueueFull)?;
        if self.inflight.len() >= self.max_inflight_frames || next_bytes > self.max_inflight_bytes {
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
            decoder,
            frame_id,
            source_timestamp_us,
            charged_bytes,
            decoded: false,
            escaped: false,
            idr: prepared.picture().idr,
        });
        self.inflight_bytes = next_bytes;
        self.last_admitted_frame = Some(frame_id);

        Ok(PreparedWebCodecsChunk {
            decoder,
            generation: decoder.generation,
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
        decoder: DecoderInstance,
        frame_id: u64,
    ) -> Result<(), BrowserHevcError> {
        self.check_live_decoder(decoder)?;
        let frame = self
            .inflight
            .iter_mut()
            .find(|f| f.decoder == decoder && f.frame_id == frame_id)
            .ok_or(BrowserHevcError::UnknownFrameId)?;
        if frame.decoded {
            return Err(BrowserHevcError::AlreadyDecoded);
        }
        if matches!(self.stage, BrowserDecoderStage::Configured { .. }) && !frame.idr {
            return Err(BrowserHevcError::AwaitingIdr);
        }
        frame.decoded = true;
        frame.escaped = true;
        if matches!(self.stage, BrowserDecoderStage::Configured { .. }) {
            self.stage = BrowserDecoderStage::FirstFrameDecoded {
                generation: decoder.generation.as_raw(),
                frame_id,
            };
        }
        Ok(())
    }

    /// Retain a native output immediately, before deciding whether its original
    /// decoder is still eligible to present. A retired decoder can still deliver
    /// a queued output; even a refused output must survive decoder shutdown in
    /// the accounting until its own `VideoFrame.close()` actually succeeds.
    pub fn retain_output(
        &mut self,
        decoder: DecoderInstance,
        frame_id: u64,
    ) -> Result<(), BrowserHevcError> {
        let frame = self
            .inflight
            .iter_mut()
            .find(|frame| frame.decoder == decoder && frame.frame_id == frame_id)
            .ok_or(BrowserHevcError::UnknownFrameId)?;
        if frame.escaped {
            return Err(BrowserHevcError::AlreadyDecoded);
        }
        frame.escaped = true;
        Ok(())
    }

    /// Record visible presentation on a non-hidden canvas (does NOT release credit until close!).
    pub fn on_frame_presented(
        &mut self,
        decoder: DecoderInstance,
        frame_id: u64,
        now_us: u64,
        visible: bool,
    ) -> Result<(), BrowserHevcError> {
        if !visible {
            return Err(BrowserHevcError::HiddenPresentationForbidden);
        }
        self.check_live_decoder(decoder)?;
        let Some(entry) = self
            .inflight
            .iter()
            .find(|f| f.decoder == decoder && f.frame_id == frame_id)
        else {
            return Err(BrowserHevcError::UnknownFrameId);
        };
        if !entry.decoded {
            return Err(BrowserHevcError::FrameNotDecoded);
        }
        if self
            .last_presented_frame
            .is_some_and(|last| frame_id <= last)
            || matches!(self.stage, BrowserDecoderStage::Presented { last_presented_us, .. } if now_us < last_presented_us)
        {
            return Err(BrowserHevcError::InvalidPresentationOrder);
        }
        self.stage = BrowserDecoderStage::Presented {
            generation: decoder.generation.as_raw(),
            last_frame_id: frame_id,
            last_source_us: entry.source_timestamp_us,
            last_presented_us: now_us,
        };
        self.last_presented_frame = Some(frame_id);
        Ok(())
    }

    /// Record that `VideoFrame.close()` freed the frame, returning the freed byte count
    /// so the caller can advance `0x0081 ReceiverPressure` cumulative credit.
    pub fn on_video_frame_closed(
        &mut self,
        decoder: DecoderInstance,
        frame_id: u64,
    ) -> Result<DecodedFrameRelease, BrowserHevcError> {
        let pos = self
            .inflight
            .iter()
            .position(|f| f.decoder == decoder && f.frame_id == frame_id)
            .ok_or(BrowserHevcError::UnknownFrameId)?;
        let entry = self
            .inflight
            .remove(pos)
            .ok_or(BrowserHevcError::UnknownFrameId)?;
        self.inflight_bytes -= entry.charged_bytes;
        self.forget_closed_decoders();
        Ok(DecodedFrameRelease {
            decoder: entry.decoder,
            frame_id: entry.frame_id,
            freed_bytes: entry.charged_bytes,
            remaining_inflight_frames: self.inflight.len(),
            remaining_inflight_bytes: self.inflight_bytes,
        })
    }

    /// Fence the decoder after `WebCodecs` reclamation or decode error; requires fresh recovery IDR.
    pub fn on_decoder_reclaimed(
        &mut self,
        decoder: DecoderInstance,
    ) -> Result<(), BrowserHevcError> {
        if self.active_decoder != Some(decoder) {
            return Err(BrowserHevcError::StaleGeneration);
        }
        self.fence_active_decoder();
        Ok(())
    }

    /// Fence the current decoder on an owner-scoped lifecycle loss. Neither
    /// this transition nor a subsequent configuration claims resource release.
    pub fn fence_active_decoder(&mut self) {
        if let Some(decoder) = self.active_decoder {
            self.awaiting_idr = true;
            self.stage = BrowserDecoderStage::NeedsRecovery {
                generation: decoder.generation.as_raw(),
            };
        }
    }

    /// Report that the ORIGINAL `VideoDecoder.close()`/`reset()` has completed
    /// and its canceled compressed-input allocations have also been released.
    /// Pending submissions then release their input charge. Output `VideoFrame`
    /// objects already handed to the application remain charged until each is
    /// explicitly closed. The adapter must report every output synchronously
    /// when received, and close refused stale outputs before reporting shutdown.
    /// Calling this after a failed or merely requested close is not permitted.
    pub fn on_decoder_closed(
        &mut self,
        decoder: DecoderInstance,
    ) -> Result<Vec<DecodedFrameRelease>, BrowserHevcError> {
        let Some(owner) = self
            .decoders
            .iter_mut()
            .find(|owner| owner.instance == decoder)
        else {
            return Err(BrowserHevcError::StaleGeneration);
        };
        owner.closed = true;
        if self.active_decoder == Some(decoder) {
            self.fence_active_decoder();
        }
        let mut releases = Vec::with_capacity(self.inflight.len());
        while let Some(pos) = self
            .inflight
            .iter()
            .position(|f| f.decoder == decoder && !f.escaped)
        {
            let entry = self
                .inflight
                .remove(pos)
                .ok_or(BrowserHevcError::UnknownFrameId)?;
            self.inflight_bytes -= entry.charged_bytes;
            releases.push(DecodedFrameRelease {
                decoder,
                frame_id: entry.frame_id,
                freed_bytes: entry.charged_bytes,
                remaining_inflight_frames: self.inflight.len(),
                remaining_inflight_bytes: self.inflight_bytes,
            });
        }
        self.forget_closed_decoders();
        Ok(releases)
    }

    fn check_live_decoder(&self, decoder: DecoderInstance) -> Result<(), BrowserHevcError> {
        if self.active_decoder != Some(decoder) {
            return Err(BrowserHevcError::StaleGeneration);
        }
        if matches!(
            self.stage,
            BrowserDecoderStage::AwaitingConfig | BrowserDecoderStage::NeedsRecovery { .. }
        ) {
            return Err(BrowserHevcError::NotConfigured);
        }
        Ok(())
    }

    fn forget_closed_decoders(&mut self) {
        self.decoders.retain(|owner| {
            !owner.closed
                || self
                    .inflight
                    .iter()
                    .any(|frame| frame.decoder == owner.instance)
        });
    }
}
