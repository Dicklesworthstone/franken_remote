//! Windows host capture (fr-p0-media-windows-fbt, fr-p2-host-windows-rt1):
//! one display through DXGI Desktop Duplication on the adapter that drives
//! it, converted to NV12 by that adapter's D3D11 video processor and encoded
//! to HEVC by the hardware Media Foundation encoder registered for the same
//! adapter. The picture stays in GPU memory until the encoder's bitstream.
//! Every unit passes the same strict `HevcGuard` as the Linux and macOS
//! encoders (after the host writes the colour description an encoder left
//! out), and leaves as four-byte-length NALs.
//!
//! The pointer is drawn into the picture, as on macOS. `Unchanged` is
//! returned only on Desktop Duplication's own evidence: no new desktop image
//! since the last encoded one, and the pointer where it was drawn. Rotated
//! displays are refused for now.
mod convert;
mod duplication;
mod encoder;
mod parent;

use crate::NativeError;
use fr_core::{ids::RecoveryGeneration, limits::ProtocolLimits};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId, FrameKind},
    config::{CodecConfiguration, ColorInfo},
    hevc::{
        HevcGuard, describe_encoder_colour_annex_b, framing::annex_b_to_length_prefixed,
        strip_encoder_sei,
    },
    worker::UnchangedCapture,
};
use std::time::{Duration, Instant};

pub use duplication::{WinDisplay, displays};
pub use encoder::EncoderIdentity;
pub use parent::bind_parent;

/// Bound for the first desktop image after opening.
const FIRST_IMAGE_MS: u32 = 1500;
/// Bound for one encode.
const ENCODE_TIMEOUT: Duration = Duration::from_millis(1500);

/// The result of one capture request.
#[derive(Debug)]
pub enum Captured {
    Unit(EncodedAccessUnit),
    Unchanged(UnchangedCapture),
}

/// Content-free work counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureStatistics {
    /// New desktop images taken from Desktop Duplication.
    pub images: u64,
    /// Duplications recreated after the desktop switched or the mode reset.
    pub recreated: u64,
    /// Times the input desktop refused a new duplication (a UAC prompt or
    /// Ctrl+Alt+Del) and the last image stood until it came back.
    pub refused_desktops: u64,
    /// Encoded units, and how many of them were IDR pictures.
    pub encoded: u64,
    pub idr: u64,
    /// IDR pictures the transform produced without being asked.
    pub unrequested_idr: u64,
    /// Units for a pointer change alone (no new desktop image).
    pub pointer_only: u64,
    /// Requests answered `Unchanged`.
    pub unchanged: u64,
}

/// Where the last encoded unit's time went, in microseconds of this
/// thread's wall clock. GPU work is charged to the call that waited for it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageTimes {
    /// Desktop Duplication: acquire, copy, release.
    pub poll: u32,
    /// Copy and pointer drawing.
    pub compose: u32,
    /// BGRA to NV12 submission.
    pub convert: u32,
    /// Submit to the encoder until its unit arrived.
    pub encode: u32,
    /// SEI removal, colour description, admission, framing.
    pub admit: u32,
}

fn micros(from: Instant, to: Instant) -> u32 {
    u32::try_from(to.saturating_duration_since(from).as_micros()).unwrap_or(u32::MAX)
}

/// Capture and encode of one display. Single owner, single thread.
pub struct WinCapture {
    source: duplication::Duplicator,
    converter: convert::Converter,
    encoder: encoder::Encoder,
    config: CodecConfiguration,
    limits: ProtocolLimits,
    admission: HevcGuard,
    last_output: Option<FrameId>,
    last_request: Option<FrameId>,
    last_observed: Option<u64>,
    /// The pointer as drawn into the last encoded picture.
    drawn: Option<duplication::Pointer>,
    bitrate: u32,
    statistics: CaptureStatistics,
    stages: StageTimes,
    /// Why the last unit was refused: the check and the NAL types it held.
    /// Content-free (no payload bytes).
    refusal: Option<String>,
    closed: bool,
}

/// The NAL unit types of an Annex B unit, in order.
fn nal_types(bytes: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 < bytes.len() {
        let start = if bytes[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if bytes[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        if let Some(header) = bytes.get(i + start) {
            types.push((header >> 1) & 0x3f);
        }
        i += start + 1;
    }
    types
}

/// Whether an Annex B unit's picture is IDR (NAL types 19 or 20).
fn annex_b_idr(bytes: &[u8]) -> Result<bool, NativeError> {
    let mut idr = None;
    let mut i = 0;
    while i + 3 < bytes.len() {
        let start = if bytes[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if bytes[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        let header = bytes
            .get(i + start)
            .ok_or(NativeError::UnsupportedBitstream)?;
        let kind = (header >> 1) & 0x3f;
        if kind < 32 {
            let this = matches!(kind, 19 | 20);
            if idr.is_some_and(|seen| seen != this) {
                return Err(NativeError::UnsupportedBitstream);
            }
            idr = Some(this);
        }
        i += start + 1;
    }
    idr.ok_or(NativeError::UnsupportedBitstream)
}

impl WinCapture {
    /// Captures `display` with the coded geometry of `config`, at `fps` and
    /// `bitrate`. The display's size must equal the visible picture.
    pub fn open(
        display: &WinDisplay,
        config: CodecConfiguration,
        limits: ProtocolLimits,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self, NativeError> {
        let g = config.geometry();
        if config.color() != ColorInfo::sdr_bt709()
            || !(1..=240).contains(&fps)
            || !(10_000..=200_000_000).contains(&bitrate)
            || (g.crop_width(), g.crop_height()) != (display.width, display.height)
        {
            return Err(NativeError::InvalidConfiguration);
        }
        let admission =
            HevcGuard::new(config, limits, 4).map_err(|_| NativeError::InvalidConfiguration)?;
        let source = duplication::Duplicator::open(display)?;
        let picture = (g.crop_width(), g.crop_height());
        let coded = (g.coded_width(), g.coded_height());
        let converter =
            convert::Converter::open(source.device(), source.composed(), picture, coded, fps)?;
        let encoder = encoder::Encoder::open(
            source.device(),
            display.adapter,
            coded,
            picture,
            fps,
            bitrate,
        )?;
        Ok(Self {
            source,
            converter,
            encoder,
            config,
            limits,
            admission,
            last_output: None,
            last_request: None,
            last_observed: None,
            drawn: None,
            bitrate,
            statistics: CaptureStatistics::default(),
            stages: StageTimes::default(),
            refusal: None,
            closed: false,
        })
    }

    pub fn encoder(&self) -> &EncoderIdentity {
        self.encoder.identity()
    }

    #[must_use]
    pub const fn bitrate(&self) -> u32 {
        self.bitrate
    }

    /// Why the capture closed on a refused unit, if it did.
    pub fn refusal(&self) -> Option<&str> {
        self.refusal.as_deref()
    }

    pub fn statistics(&self) -> CaptureStatistics {
        CaptureStatistics {
            images: self.source.acquired,
            recreated: self.source.recreated,
            refused_desktops: self.source.refused_desktops,
            ..self.statistics
        }
    }

    /// Stage durations of the last encoded unit.
    pub const fn stages(&self) -> StageTimes {
        self.stages
    }

    /// Sets a new mean bitrate (at most the opening one) for later pictures.
    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<u32, NativeError> {
        if !(10_000..=200_000_000).contains(&bitrate) {
            return Err(NativeError::InvalidConfiguration);
        }
        self.bitrate = self.encoder.set_bitrate(bitrate)?;
        Ok(self.bitrate)
    }

    /// Captures the display after this request and encodes it as `frame`.
    /// `observed_micros` is the parent's pre-request lower bound. A failure
    /// closes the capture: no later request can splice a broken chain.
    pub fn capture(
        &mut self,
        frame: FrameId,
        observed_micros: u64,
        force_idr: bool,
        only_if_changed: bool,
    ) -> Result<Captured, NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        if self.last_request.is_some_and(|last| frame <= last)
            || self
                .last_observed
                .is_some_and(|last| observed_micros < last)
        {
            self.closed = true;
            return Err(NativeError::StaleGeneration);
        }
        self.last_request = Some(frame);
        self.last_observed = Some(observed_micros);
        let result = self.capture_inner(frame, observed_micros, force_idr, only_if_changed);
        if result.is_err() {
            self.closed = true;
        }
        result
    }

    fn capture_inner(
        &mut self,
        frame: FrameId,
        observed_micros: u64,
        force_idr: bool,
        only_if_changed: bool,
    ) -> Result<Captured, NativeError> {
        let force_idr = force_idr || self.last_output.is_none();
        let wait = if self.source.has_picture() {
            0
        } else {
            FIRST_IMAGE_MS
        };
        let began = Instant::now();
        let changed = self.source.poll(wait)?;
        let polled = Instant::now();
        if !self.source.has_picture() {
            return Err(NativeError::Unavailable);
        }
        let pointer = self.source.pointer();
        let pointer_moved = self.drawn != Some(pointer);
        if only_if_changed && !force_idr && !changed && !pointer_moved {
            let reference = self.last_output.ok_or(NativeError::Codec)?;
            self.statistics.unchanged += 1;
            return Ok(Captured::Unchanged(UnchangedCapture {
                candidate: frame,
                reference,
                observed_micros,
            }));
        }
        let composing = Instant::now();
        self.source.compose(pointer)?;
        let composed = Instant::now();
        self.converter.convert()?;
        let converted = Instant::now();
        let (bytes, clean) = self.encoder.encode(
            self.converter.output(),
            force_idr,
            observed_micros,
            converted + ENCODE_TIMEOUT,
        )?;
        let encoded = Instant::now();
        let types = nal_types(&bytes);
        let mut refuse = |check: String| {
            self.refusal = Some(format!("{check} nal_types={types:?} clean_point={clean}"));
            NativeError::UnsupportedBitstream
        };
        // Encoder metadata SEI is not needed to decode; admission refuses
        // anything else outside the baseline.
        let bytes = strip_encoder_sei(&bytes).map_err(|e| refuse(format!("strip_sei:{e:?}")))?;
        let idr = annex_b_idr(&bytes).map_err(|_| refuse("picture_type".into()))?;
        if idr != clean || (force_idr && !idr) {
            return Err(refuse(format!("idr={idr} forced={force_idr}")));
        }
        // Intel's transform signals studio range but no primaries, transfer
        // or matrix, and refuses the settings that would add them. Its input
        // is this converter's BT.709 output, so the SPS says exactly that;
        // admission then checks the result like any encoder's.
        let bytes = if idr {
            describe_encoder_colour_annex_b(&bytes, self.config.color())
                .map_err(|e| refuse(format!("colour:{e:?}")))?
        } else {
            bytes
        };
        if let Err(e) = self.admission.validate_annex_b(&bytes, idr) {
            return Err(refuse(format!("admission:{e:?}")));
        }
        let bytes = annex_b_to_length_prefixed(&bytes, self.limits)
            .map_err(|_| NativeError::UnsupportedBitstream)?;
        let kind = if idr {
            FrameKind::Idr {
                recovery: RecoveryGeneration::INITIAL,
            }
        } else {
            FrameKind::Predicted {
                references: self.last_output.ok_or(NativeError::UnsupportedBitstream)?,
            }
        };
        let unit = EncodedAccessUnit::new(
            &self.limits,
            frame,
            kind,
            self.config.generation(),
            observed_micros,
            bytes,
        )
        .map_err(|_| NativeError::Codec)?;
        self.statistics.encoded += 1;
        if idr {
            self.statistics.idr += 1;
            if !force_idr {
                self.statistics.unrequested_idr += 1;
            }
        }
        if !changed {
            self.statistics.pointer_only += 1;
        }
        self.last_output = Some(frame);
        self.drawn = Some(pointer);
        self.stages = StageTimes {
            poll: micros(began, polled),
            compose: micros(composing, composed),
            convert: micros(composed, converted),
            encode: micros(converted, encoded),
            admit: micros(encoded, Instant::now()),
        };
        Ok(Captured::Unit(unit))
    }
}

#[cfg(test)]
mod tests {
    use super::annex_b_idr;
    use crate::NativeError;

    #[test]
    fn idr_comes_from_the_slice_nal_type() {
        // VPS, SPS, PPS then an IDR_W_RADL slice (type 19).
        let idr = [
            0, 0, 0, 1, 0x40, 1, 0xaa, 0, 0, 0, 1, 0x42, 1, 0xbb, 0, 0, 1, 0x44, 1, 0xcc, 0, 0, 0,
            1, 0x26, 1, 0xdd,
        ];
        assert_eq!(annex_b_idr(&idr), Ok(true));
        // A TRAIL_R slice (type 1) is not.
        assert_eq!(annex_b_idr(&[0, 0, 0, 1, 0x02, 1, 0xee, 0xff]), Ok(false));
        // Planted negatives: no slice at all, and slices that disagree.
        assert_eq!(
            annex_b_idr(&[0, 0, 0, 1, 0x40, 1, 0xaa, 0xbb]),
            Err(NativeError::UnsupportedBitstream)
        );
        assert_eq!(
            annex_b_idr(&[0, 0, 0, 1, 0x26, 1, 0xaa, 0, 0, 0, 1, 0x02, 1, 0xbb]),
            Err(NativeError::UnsupportedBitstream)
        );
    }
}
