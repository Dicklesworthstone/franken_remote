//! Publish only packets whose original native-frame evidence survives body
//! construction. The caller retains those frame owners through this function;
//! this is a producer bound, never observation or remote-session authority.
#![forbid(unsafe_code)]
use fr_core::limits::ProtocolLimits;
use fr_media::{
    audio::AudioAccessUnit,
    worker::{
        Error, Identity, Kind, Record,
        audio::{Capture, CaptureCounters, encode_batch},
    },
};
use std::io::Write;

pub const MAX_FRAMES: usize = 4;

/// The final guard validates the original source, then checks every original
/// frame against one post-native clock sample. No native call separates those
/// deadline comparisons. A false mask entry drops just that expired packet.
/// Every rebuild removes at least one of four packets; no serialization delay
/// can renew a source deadline or create a retry loop.
/// Returns the additional discarded-frame count after a successful write.
pub fn publish(
    capture: &Capture,
    identity: Identity,
    packets: Vec<(usize, AudioAccessUnit)>,
    mut counters: CaptureCounters,
    output: &mut impl Write,
    mut final_check: impl FnMut() -> Result<[bool; MAX_FRAMES], Error>,
) -> Result<u32, Error> {
    if packets.len() > MAX_FRAMES {
        return Err(Error::ResourceLimit);
    }
    if packets.iter().any(|(origin, _)| *origin >= MAX_FRAMES) {
        return Err(Error::Malformed);
    }
    let (mut origins, mut packets): (Vec<_>, Vec<_>) = packets.into_iter().unzip();
    let mut discarded = 0_u32;
    loop {
        let body = encode_batch(&packets, counters, capture)?;
        let record = Record::new(
            Kind::AudioPackets,
            identity,
            body,
            &ProtocolLimits::ABSOLUTE,
        )?;
        // A callback may have ended the source while copying the body. The
        // complete mask follows that native query; checking another frame
        // cannot consume an earlier frame's already-checked lifetime.
        let live = final_check()?;
        let before = packets.len();
        for index in (0..before).rev() {
            if !live[origins[index]] {
                origins.remove(index);
                packets.remove(index);
                discarded = discarded.saturating_add(1);
                counters.gaps = counters.gaps.saturating_add(1);
            }
        }
        if packets.len() != before {
            continue;
        }
        record.write(output, &ProtocolLimits::ABSOLUTE)?;
        return Ok(discarded);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration};
    use fr_media::worker::audio::{Source, decode_batch};
    use std::cell::Cell;

    fn capture() -> Capture {
        Capture {
            generation: AudioGeneration::from_raw(4),
            channels: AudioChannels::Stereo,
            frame_duration_ms: 20,
            bitrate: 96_000,
            max_packet_bytes: 1000,
            source: Source::MacSystem,
        }
    }
    fn packets(count: usize) -> Vec<(usize, AudioAccessUnit)> {
        (0..count)
            .map(|index| {
                let sequence = u64::try_from(index).unwrap();
                (
                    index,
                    AudioAccessUnit::new(
                        AudioDirection::Downlink,
                        AudioGeneration::from_raw(4),
                        sequence,
                        sequence * 960,
                        960,
                        false,
                        &[0x5a],
                    )
                    .unwrap(),
                )
            })
            .collect()
    }
    const IDENTITY: Identity = Identity {
        epoch: 7,
        sequence: 2,
    };

    #[test]
    fn final_source_check_cannot_leave_an_expired_encoded_packet_in_the_body() {
        let time = Cell::new(0);
        let checks = Cell::new(0);
        let mut output = Vec::new();
        let discarded = publish(
            &capture(),
            IDENTITY,
            packets(3),
            CaptureCounters::default(),
            &mut output,
            || {
                checks.set(checks.get() + 1);
                time.set(2);
                // Original middle frame expires while preparing the batch.
                Ok(std::array::from_fn(|original| {
                    time.get() < if original == 1 { 1 } else { 10 }
                }))
            },
        )
        .unwrap();
        assert_eq!(discarded, 1);
        assert_eq!(checks.get(), 2);
        let record = Record::read(&mut output.as_slice(), &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .unwrap();
        assert_eq!(record.header.identity, IDENTITY);
        let batch = decode_batch(record.body(), &capture()).unwrap();
        assert_eq!(batch.counters.gaps, 1);
        assert_eq!(
            batch
                .packets
                .iter()
                .map(AudioAccessUnit::sequence)
                .collect::<Vec<_>>(),
            [0, 2]
        );
    }

    #[test]
    fn each_expiry_rebuild_consumes_a_packet_and_never_refreshes_its_deadline() {
        let time = Cell::new(0);
        let mut output = Vec::new();
        let discarded = publish(
            &capture(),
            IDENTITY,
            packets(4),
            CaptureCounters::default(),
            &mut output,
            || {
                time.set(time.get() + 1);
                Ok(std::array::from_fn(|original| time.get() < original + 1))
            },
        )
        .unwrap();
        assert_eq!(discarded, 4);
        assert_eq!(time.get(), 5);
        let record = Record::read(&mut output.as_slice(), &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .unwrap();
        let batch = decode_batch(record.body(), &capture()).unwrap();
        assert_eq!(batch.packets, [] as [AudioAccessUnit; 0]);
        assert_eq!(batch.counters.gaps, 4);
    }

    #[test]
    fn terminal_source_or_frame_owner_error_never_writes_a_partial_batch() {
        for error in [Error::NativeFailure, Error::Malformed] {
            let mut output = Vec::new();
            assert_eq!(
                publish(
                    &capture(),
                    IDENTITY,
                    packets(4),
                    CaptureCounters::default(),
                    &mut output,
                    || Err(error)
                ),
                Err(error)
            );
            assert_eq!(output, [] as [u8; 0]);
        }
        assert_eq!(
            publish(
                &capture(),
                IDENTITY,
                packets(5),
                CaptureCounters::default(),
                &mut Vec::new(),
                || panic!("oversized batch reached native source")
            ),
            Err(Error::ResourceLimit)
        );
        let mut invalid = packets(1);
        invalid[0].0 = MAX_FRAMES;
        assert_eq!(
            publish(
                &capture(),
                IDENTITY,
                invalid,
                CaptureCounters::default(),
                &mut Vec::new(),
                || panic!("invalid original index reached native source")
            ),
            Err(Error::Malformed)
        );
    }
}
