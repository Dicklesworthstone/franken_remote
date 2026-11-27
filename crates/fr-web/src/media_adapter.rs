//! Native `WebCodecs` custody at the WASM boundary. This owner grants no session,
//! observation, freshness or input authority and emits no wire credit. An
//! authenticated session must separately route its original release receipts.
//!
//! JavaScript owns browser handles only. Rust validates the exact HEVC record,
//! admits complete access units, assigns non-reused callback timestamps, and
//! retains every charge until positive original decoder/output closure.

use crate::hevc::{
    BrowserDecoderStage, BrowserHevcError, BrowserHevcPipeline, DecodedFrameRelease,
    DecoderInstance, MAX_BROWSER_INFLIGHT_FRAMES, PreparedWebCodecsChunk,
};
use fr_core::{ids::CodecConfigurationGeneration, limits::ProtocolLimits};
use fr_media::{
    config::{
        CodecConfiguration, CodedGeometry, ColorInfo, ColorMatrix, ColorPrimaries, ColorRange,
        GopPolicy, TransferFunction,
    },
    hevc::HevcGuard,
};
use serde::{Deserialize, Serialize};

pub const MEDIA_ABI_VERSION: u32 = 1;
pub const MAX_CONFIGURATION_METADATA_BYTES: usize = 1024;
pub const MAX_CONFIGURATION_BYTES: usize = fr_wire::decoder::MAX_HVCC_BYTES;
pub const MAX_ACCESS_UNIT_BYTES: usize = 16 * 1024 * 1024;
const MAX_CALLBACK: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaAdapterError {
    Configuration,
    Bounds,
    Identity,
    Clock,
    State,
    Quarantined,
    Hevc(BrowserHevcError),
}
impl MediaAdapterError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Configuration => "browser_media_configuration_invalid",
            Self::Bounds => "browser_media_bounds_exceeded",
            Self::Identity => "browser_media_owner_invalid",
            Self::Clock => "browser_media_clock_invalid",
            Self::State => "browser_media_state_invalid",
            Self::Quarantined => "browser_media_cleanup_incomplete",
            Self::Hevc(BrowserHevcError::DecodeQueueFull) => "browser_media_capacity",
            Self::Hevc(BrowserHevcError::DecoderCapacity) => "browser_decoder_capacity",
            Self::Hevc(BrowserHevcError::StaleGeneration) => "browser_media_owner_retired",
            Self::Hevc(_) => "browser_hevc_admission_refused",
        }
    }
}
impl From<BrowserHevcError> for MediaAdapterError {
    fn from(value: BrowserHevcError) -> Self {
        Self::Hevc(value)
    }
}

/// This is a local ABI projection of an already received configuration, not a
/// second wire protocol or a caller-created `SessionOpened`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ConfigurationMetadata<'a> {
    generation: &'a str,
    coded_width: u32,
    coded_height: u32,
    crop_width: u32,
    crop_height: u32,
    fps: u16,
    primaries: u8,
    transfer: u8,
    matrix: u8,
    full_range: bool,
    decoded_pictures: u8,
    codec: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedConfiguration {
    pub decoder: String,
    pub generation: String,
    pub codec: String,
    pub hvcc: Vec<u8>,
    pub coded_width: u32,
    pub coded_height: u32,
    pub crop_width: u32,
    pub crop_height: u32,
}

#[derive(Clone, Copy)]
struct FrameOwner {
    decoder: DecoderInstance,
    callback: u64,
    frame: u64,
    source: u64,
    charged: u64,
    escaped: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputReceipt {
    pub decoder: String,
    pub generation: String,
    pub callback: u64,
    pub frame_id: String,
    pub source_timestamp_us: String,
    pub charged_bytes: u64,
    pub can_present: bool,
    pub first_decoded: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseReceipt {
    pub decoder: String,
    pub generation: String,
    pub callback: u64,
    pub frame_id: String,
    pub released_bytes: u64,
    pub remaining_frames: usize,
    pub remaining_bytes: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaSnapshot {
    pub active_decoder: Option<String>,
    pub retained_decoders: usize,
    pub inflight_frames: usize,
    pub inflight_bytes: u64,
    pub stage: &'static str,
    pub quarantined: bool,
}

pub struct PreparedChunk {
    pub callback: u64,
    pub inner: PreparedWebCodecsChunk,
}

pub struct MediaAdapter {
    pipeline: BrowserHevcPipeline,
    decoders: Vec<DecoderInstance>,
    frames: Vec<FrameOwner>,
    next_callback: u64,
    last_presented_us: u64,
    quarantined: bool,
}

impl MediaAdapter {
    pub fn new(max_frames: usize, max_bytes: u64) -> Self {
        Self {
            pipeline: BrowserHevcPipeline::new(
                max_frames,
                max_bytes.min(ProtocolLimits::ABSOLUTE.per_viewer_compressed_bytes()),
            ),
            decoders: Vec::with_capacity(crate::hevc::MAX_BROWSER_DECODERS),
            frames: Vec::with_capacity(MAX_BROWSER_INFLIGHT_FRAMES),
            next_callback: 0,
            last_presented_us: 0,
            quarantined: false,
        }
    }

    fn usable(&self) -> Result<(), MediaAdapterError> {
        if self.quarantined {
            Err(MediaAdapterError::Quarantined)
        } else {
            Ok(())
        }
    }

    /// A native callback escaped without a provable original owner and could
    /// not be closed. No bounded ledger can account for a broken native API's
    /// arbitrary allocations. Fence the whole media owner permanently, keep
    /// its existing reservations, and require outer-owner teardown. In
    /// particular, neither reset nor later decoder closure fabricates credit.
    pub fn quarantine(&mut self) {
        self.quarantined = true;
        self.pipeline.fence_active_decoder();
    }

    pub fn prepare_configuration(
        &mut self,
        metadata: &str,
        hvcc: &[u8],
    ) -> Result<PreparedConfiguration, MediaAdapterError> {
        self.usable()?;
        if metadata.len() > MAX_CONFIGURATION_METADATA_BYTES || hvcc.len() > MAX_CONFIGURATION_BYTES
        {
            return Err(MediaAdapterError::Bounds);
        }
        let meta: ConfigurationMetadata<'_> =
            serde_json::from_str(metadata).map_err(|_| MediaAdapterError::Configuration)?;
        let generation = decimal(meta.generation)?;
        let declaration = fr_wire::decoder::Configuration {
            coded_width: meta.coded_width,
            coded_height: meta.coded_height,
            crop_width: meta.crop_width,
            crop_height: meta.crop_height,
            fps: meta.fps,
            primaries: meta.primaries,
            transfer: meta.transfer,
            matrix: meta.matrix,
            full_range: meta.full_range,
            decoded_pictures: meta.decoded_pictures,
            codec: meta.codec,
            hvcc,
        };
        declaration
            .validate(&ProtocolLimits::ABSOLUTE)
            .map_err(|_| MediaAdapterError::Configuration)?;
        let config = checked_configuration(declaration, generation, ProtocolLimits::ABSOLUTE)?;
        let prepared = self.pipeline.admit_decoder_configuration(
            config,
            ProtocolLimits::ABSOLUTE,
            meta.decoded_pictures,
            hvcc,
        )?;
        self.decoders.push(prepared.decoder);
        Ok(PreparedConfiguration {
            decoder: prepared.decoder.serial.to_string(),
            generation: generation.to_string(),
            codec: prepared.codec_string,
            hvcc: prepared.hvcc,
            coded_width: prepared.coded_width,
            coded_height: prepared.coded_height,
            crop_width: prepared.crop_width,
            crop_height: prepared.crop_height,
        })
    }

    fn decoder(&self, serial: u64) -> Result<DecoderInstance, MediaAdapterError> {
        self.decoders
            .iter()
            .copied()
            .find(|owner| owner.serial == serial)
            .ok_or(MediaAdapterError::Identity)
    }

    pub fn configured(&mut self, serial: u64) -> Result<(), MediaAdapterError> {
        self.usable()?;
        Ok(self.pipeline.on_decoder_configured(self.decoder(serial)?)?)
    }

    pub fn prepare_chunk(
        &mut self,
        serial: u64,
        frame: u64,
        source: u64,
        key: bool,
        bytes: &[u8],
    ) -> Result<PreparedChunk, MediaAdapterError> {
        self.usable()?;
        let decoder = self.decoder(serial)?;
        if bytes.len() > MAX_ACCESS_UNIT_BYTES || self.next_callback >= MAX_CALLBACK {
            return Err(MediaAdapterError::Bounds);
        }
        let inner = self
            .pipeline
            .admit_access_unit(decoder, frame, source, key, bytes)?;
        self.next_callback += 1;
        let callback = self.next_callback;
        self.frames.push(FrameOwner {
            decoder,
            callback,
            frame,
            source,
            charged: inner.charged_bytes,
            escaped: false,
        });
        Ok(PreparedChunk { callback, inner })
    }

    fn frame(&self, serial: u64, callback: u64) -> Result<FrameOwner, MediaAdapterError> {
        self.frames
            .iter()
            .copied()
            .find(|frame| frame.decoder.serial == serial && frame.callback == callback)
            .ok_or(MediaAdapterError::Identity)
    }

    pub fn output(
        &mut self,
        serial: u64,
        callback: u64,
    ) -> Result<OutputReceipt, MediaAdapterError> {
        self.usable()?;
        let frame = self.frame(serial, callback)?;
        self.pipeline.retain_output(frame.decoder, frame.frame)?;
        self.frames
            .iter_mut()
            .find(|owner| owner.decoder.serial == serial && owner.callback == callback)
            .ok_or(MediaAdapterError::Identity)?
            .escaped = true;
        let was_configured = matches!(
            self.pipeline.stage(),
            BrowserDecoderStage::Configured { .. }
        );
        let can_present = self
            .pipeline
            .on_frame_decoded(frame.decoder, frame.frame)
            .is_ok();
        Ok(OutputReceipt {
            decoder: serial.to_string(),
            generation: frame.decoder.generation.as_raw().to_string(),
            callback,
            frame_id: frame.frame.to_string(),
            source_timestamp_us: frame.source.to_string(),
            charged_bytes: frame.charged,
            can_present,
            first_decoded: can_present && was_configured,
        })
    }

    pub fn presented(
        &mut self,
        serial: u64,
        callback: u64,
        now_us: u64,
        visible: bool,
    ) -> Result<(), MediaAdapterError> {
        self.usable()?;
        let frame = self.frame(serial, callback)?;
        if now_us < self.last_presented_us {
            return Err(MediaAdapterError::Clock);
        }
        self.pipeline
            .on_frame_presented(frame.decoder, frame.frame, now_us, visible)?;
        self.last_presented_us = now_us;
        Ok(())
    }

    pub fn frame_closed(
        &mut self,
        serial: u64,
        callback: u64,
    ) -> Result<ReleaseReceipt, MediaAdapterError> {
        self.usable()?;
        let frame = self.frame(serial, callback)?;
        if !frame.escaped {
            return Err(MediaAdapterError::State);
        }
        let release = self
            .pipeline
            .on_video_frame_closed(frame.decoder, frame.frame)?;
        self.release(release)
    }

    fn release(
        &mut self,
        released: DecodedFrameRelease,
    ) -> Result<ReleaseReceipt, MediaAdapterError> {
        let position = self
            .frames
            .iter()
            .position(|frame| frame.decoder == released.decoder && frame.frame == released.frame_id)
            .ok_or(MediaAdapterError::State)?;
        let frame = self.frames.remove(position);
        self.decoders
            .retain(|decoder| self.pipeline.contains_decoder(*decoder));
        Ok(ReleaseReceipt {
            decoder: released.decoder.serial.to_string(),
            generation: released.decoder.generation.as_raw().to_string(),
            callback: frame.callback,
            frame_id: released.frame_id.to_string(),
            released_bytes: released.freed_bytes,
            remaining_frames: released.remaining_inflight_frames,
            remaining_bytes: released.remaining_inflight_bytes,
        })
    }

    pub fn reclaim(&mut self, serial: u64) -> Result<(), MediaAdapterError> {
        Ok(self.pipeline.on_decoder_reclaimed(self.decoder(serial)?)?)
    }

    pub fn retire(&mut self) {
        self.pipeline.fence_active_decoder();
    }

    pub fn decoder_closed(
        &mut self,
        serial: u64,
    ) -> Result<Vec<ReleaseReceipt>, MediaAdapterError> {
        self.usable()?;
        let released = self.pipeline.on_decoder_closed(self.decoder(serial)?)?;
        let receipts = released
            .into_iter()
            .map(|release| self.release(release))
            .collect();
        self.decoders
            .retain(|decoder| self.pipeline.contains_decoder(*decoder));
        receipts
    }

    pub fn reset_session(&mut self) -> Result<(), MediaAdapterError> {
        self.usable()?;
        Ok(self.pipeline.reset_session()?)
    }

    pub fn snapshot(&self) -> MediaSnapshot {
        MediaSnapshot {
            active_decoder: if self.quarantined {
                None
            } else {
                self.pipeline
                    .active_decoder()
                    .map(|decoder| decoder.serial.to_string())
            },
            retained_decoders: self.pipeline.retained_decoders(),
            inflight_frames: self.pipeline.inflight_frames(),
            inflight_bytes: self.pipeline.inflight_bytes(),
            quarantined: self.quarantined,
            stage: if self.quarantined {
                "quarantined"
            } else {
                match self.pipeline.stage() {
                    BrowserDecoderStage::AwaitingConfig => "awaiting_configuration",
                    BrowserDecoderStage::Configured { .. } => "configured",
                    BrowserDecoderStage::FirstFrameDecoded { .. } => "decoded",
                    BrowserDecoderStage::Presented { .. } => "submitted_to_canvas",
                    BrowserDecoderStage::NeedsRecovery { .. } => "needs_recovery",
                }
            },
        }
    }
}

fn decimal(value: &str) -> Result<u64, MediaAdapterError> {
    if value.is_empty()
        || value.len() > 20
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(MediaAdapterError::Configuration);
    }
    value.parse().map_err(|_| MediaAdapterError::Configuration)
}

/// Shared declaration/SPS validation for the local probe and authenticated
/// browser session. JavaScript never chooses a codec string independently.
pub(crate) fn checked_configuration(
    declaration: fr_wire::decoder::Configuration<'_>,
    generation: u64,
    limits: ProtocolLimits,
) -> Result<CodecConfiguration, MediaAdapterError> {
    declaration
        .validate(&limits)
        .map_err(|_| MediaAdapterError::Configuration)?;
    // This browser lane admits a bounded low-delay four-picture DPB. It
    // does not promise that a native implementation's hidden pools qualify.
    if declaration.decoded_pictures > 4 {
        return Err(MediaAdapterError::Bounds);
    }
    let geometry = CodedGeometry::new(
        &limits,
        declaration.coded_width,
        declaration.coded_height,
        declaration.crop_width,
        declaration.crop_height,
        2,
    )
    .map_err(|_| MediaAdapterError::Configuration)?;
    let color = ColorInfo {
        primaries: if declaration.primaries == 1 {
            ColorPrimaries::Bt709
        } else {
            ColorPrimaries::Bt2020
        },
        transfer: if declaration.transfer == 1 {
            TransferFunction::Bt709
        } else {
            TransferFunction::Srgb
        },
        matrix: if declaration.matrix == 1 {
            ColorMatrix::Bt709
        } else {
            ColorMatrix::Bt2020Ncl
        },
        range: if declaration.full_range {
            ColorRange::Full
        } else {
            ColorRange::Limited
        },
    };
    let config = CodecConfiguration::new_baseline(
        CodecConfigurationGeneration::from_raw(generation),
        geometry,
        color,
        GopPolicy::baseline_for_frame_rate(u32::from(declaration.fps))
            .map_err(|_| MediaAdapterError::Configuration)?,
    )
    .map_err(|_| MediaAdapterError::Configuration)?;
    // Validate the advertised codec string before mutating the live owner.
    let guard = HevcGuard::from_decoder_record(
        config,
        limits,
        declaration.decoded_pictures,
        declaration.hvcc,
    )
    .map_err(|_| MediaAdapterError::Configuration)?;
    let record = guard
        .decoder_record()
        .map_err(|_| MediaAdapterError::Configuration)?;
    if declaration.codec.strip_prefix("hev1.") != record.codec().strip_prefix("hvc1.") {
        return Err(MediaAdapterError::Configuration);
    }

    Ok(config)
}
