//! ISO/IEC 14496-15 HEVC configuration records from admitted parameter sets.
//! `hvc1` output removes in-band parameter sets; native/wire recovery can retain
//! them. No hard-coded profile, compatibility mask, constraint bytes or level.
use super::{HevcError, HevcGuard, PictureInfo, framing};
use core::fmt::{self, Write};
use fr_core::ids::CodecConfigurationGeneration;

/// Three parameter arrays with one bounded 4096-byte NAL each.
pub const MAX_DECODER_RECORD_BYTES: usize = 23 + 3 * (5 + 4096);

/// An exact `hvcC` payload (not an MP4 box) and fully qualified `hvc1` identifier.
/// The `hvc1` identifier applies only to `prepare_hvc1` samples, which remove
/// parameter sets. Native adapters may use the record bytes with their explicit
/// in-band-parameter-set contract; that does not label those samples `hvc1`.
pub struct DecoderRecord {
    bytes: Vec<u8>,
    codec: String,
    generation: CodecConfigurationGeneration,
}
impl DecoderRecord {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn codec(&self) -> &str {
        &self.codec
    }
    pub const fn generation(&self) -> CodecConfigurationGeneration {
        self.generation
    }
}
impl fmt::Debug for DecoderRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecoderRecord")
            .field("codec", &self.codec)
            .field("generation", &self.generation)
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}
/// One admitted browser sample. On every IDR, configure/confirm the returned
/// description before submitting the sample. `idr` supplies key/delta truth;
/// presentation timestamps and output-frame lifetimes belong to the caller.
pub struct Hvc1AccessUnit {
    bytes: Vec<u8>,
    picture: PictureInfo,
    configuration: Option<DecoderRecord>,
}
impl Hvc1AccessUnit {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub const fn picture(&self) -> PictureInfo {
        self.picture
    }
    pub const fn configuration(&self) -> Option<&DecoderRecord> {
        self.configuration.as_ref()
    }
}
impl fmt::Debug for Hvc1AccessUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hvc1AccessUnit")
            .field("byte_len", &self.bytes.len())
            .field("picture", &self.picture)
            .field("configuration", &self.configuration)
            .finish()
    }
}
impl HevcGuard {
    /// Admit the exact canonical configuration emitted by `decoder_record`.
    /// This establishes parameter identity, geometry, color and resource bounds,
    /// but NO decoded/reference history. The first submitted picture must be IDR.
    pub fn from_decoder_record(
        config: crate::config::CodecConfiguration,
        limits: fr_core::limits::ProtocolLimits,
        max_decoded_pictures: u8,
        record: &[u8],
    ) -> Result<Self, HevcError> {
        if record.len() > MAX_DECODER_RECORD_BYTES {
            return Err(HevcError::Limit);
        }
        limits
            .validate_control_message_len(record.len())
            .map_err(|_| HevcError::Limit)?;
        if record.len() < 23 {
            return Err(HevcError::Truncated);
        }
        let mut guard = Self::new(config, limits, max_decoded_pictures)?;
        let mut offset = 23;
        let mut sets = [None; 3];
        for (slot, kind) in sets.iter_mut().zip(32_u8..=34) {
            let header = record.get(offset..offset + 5).ok_or(HevcError::Truncated)?;
            if header[..3] != [0x80 | kind, 0, 1] {
                return Err(HevcError::UnsupportedSyntax);
            }
            let length = usize::from(u16::from_be_bytes([header[3], header[4]]));
            if !(2..=4096).contains(&length) {
                return Err(HevcError::Limit);
            }
            offset += 5;
            let nal = record
                .get(offset..offset + length)
                .ok_or(HevcError::Truncated)?;
            if super::nal::Nal::new(nal)?.kind != kind {
                return Err(HevcError::ParameterSetReference);
            }
            *slot = Some(nal);
            offset += length;
        }
        if offset != record.len() {
            return Err(HevcError::Framing);
        }
        guard.sets = Some(std::sync::Arc::new(guard.parse_sets(sets)?));
        // Compare every header field against actual admitted SPS facts and the
        // fixed baseline. No alternate lengths, extra arrays or hidden profile.
        if guard.decoder_record()?.bytes() != record {
            return Err(HevcError::UnsupportedSyntax);
        }
        Ok(guard)
    }
    /// Generates the exact configuration established by an admitted record or
    /// IDR. No configuration is fabricated from desired encoder settings.
    pub fn decoder_record(&self) -> Result<DecoderRecord, HevcError> {
        let sets = self.sets.as_ref().ok_or(HevcError::MissingParameterSet)?;
        let mut bytes = Vec::new();
        let size = 23 + sets.bytes.iter().map(|p| p.len() + 5).sum::<usize>();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| HevcError::Allocation)?;
        bytes.push(1);
        bytes.extend_from_slice(&sets.sps.profile.0);
        // Unknown minimum spatial segmentation / parallelism / average frame
        // rate; Main 4:2:0 eight-bit, one nested temporal layer, four-byte lengths.
        bytes.extend_from_slice(&[0xf0, 0, 0xfc, 0xfd, 0xf8, 0xf8, 0, 0, 0x0f, 3]);
        for (kind, nal) in [32_u8, 33, 34].into_iter().zip(&sets.bytes) {
            bytes.extend_from_slice(&[0x80 | kind, 0, 1]);
            bytes.extend_from_slice(
                &u16::try_from(nal.len())
                    .map_err(|_| HevcError::Limit)?
                    .to_be_bytes(),
            );
            bytes.extend_from_slice(nal);
        }
        let profile = sets.sps.profile.0;
        let compatibility =
            u32::from_be_bytes(profile[1..5].try_into().map_err(|_| HevcError::Truncated)?)
                .reverse_bits();
        let tier = if profile[0] & 0x20 == 0 { 'L' } else { 'H' };
        let mut codec = String::new();
        codec
            .try_reserve_exact(64)
            .map_err(|_| HevcError::Allocation)?;
        write!(
            &mut codec,
            "hvc1.{}.{compatibility:X}.{tier}{}",
            profile[0] & 31,
            profile[11]
        )
        .map_err(|_| HevcError::Allocation)?;
        let constraints = &profile[5..11];
        // Constraint bytes retain their order; only trailing zero bytes may go.
        // Keep one field even for an all-zero mask.
        let count = constraints
            .iter()
            .rposition(|b| *b != 0)
            .map_or(1, |n| n + 1);
        for byte in &constraints[..count] {
            write!(&mut codec, ".{byte:02X}").map_err(|_| HevcError::Allocation)?;
        }
        Ok(DecoderRecord {
            bytes,
            codec,
            generation: self.config.generation(),
        })
    }
    /// Validates one canonical wire AU and prepares matching `WebCodecs` bytes.
    /// Parameter sets are removed only after checking their frozen identities.
    /// Allocation failure or rejection never consumes the guard's reference state.
    pub fn prepare_hvc1(
        &mut self,
        bytes: &[u8],
        declared_idr: bool,
    ) -> Result<Hvc1AccessUnit, HevcError> {
        let mut candidate = self.clone();
        let picture = candidate.validate_length_prefixed(bytes, declared_idr)?;
        let configuration = if picture.idr {
            Some(candidate.decoder_record()?)
        } else {
            None
        };
        let bytes = framing::hvc1_sample(bytes, self.limits)?;
        *self = candidate;
        Ok(Hvc1AccessUnit {
            bytes,
            picture,
            configuration,
        })
    }
}
