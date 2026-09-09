//! Offline HEVC corpus delivery diagnostic, not a remote desktop or live transport.
//! The fixture producer and final independent decoder are in
//! `scripts/verify_hevc_delivery.py`. No native media library is linked here.
use fr_core::ids::{CodecConfigurationGeneration, RecoveryGeneration};
use fr_core::limits::ProtocolLimits;
use fr_media::delivery::{
    DeliveryMode, MediaBindings, MediaBudget, MediaEpoch, PacketOffer, ReceiveConfig,
    ReceivePipeline, ReceivePolicy, SendCache, SendPolicy,
};
use fr_wire::{Channel, FrameDescriptor, MediaLimits, PipelineState, Progress, SourceObservation};
use std::error::Error;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};

const MAGIC: &[u8; 8] = b"FRHEVC01";
const PACKET_BYTES: usize = 1_150;
const MAX_FRAMES: u32 = 1_000;
const MAX_CORPUS_BYTES: u64 = 256 * 1024 * 1024;
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn u32_from(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}
fn header(r: &mut impl Read) -> io::Result<u32> {
    let mut magic = [0; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(invalid("wrong corpus magic"));
    }
    let count = u32_from(r)?;
    if !(1..=MAX_FRAMES).contains(&count) {
        return Err(invalid("invalid corpus frame count"));
    }
    Ok(count)
}
fn picture(r: &mut impl Read, total: &mut u64) -> io::Result<(bool, Vec<u8>)> {
    let mut key = [0];
    r.read_exact(&mut key)?;
    if key[0] > 1 {
        return Err(invalid("invalid corpus picture kind"));
    }
    let size = u32_from(r)?;
    if size == 0 || size > ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes() {
        return Err(invalid("corpus picture exceeds limit"));
    }
    *total = total
        .checked_add(u64::from(size))
        .filter(|n| *n <= MAX_CORPUS_BYTES)
        .ok_or_else(|| invalid("corpus exceeds byte limit"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size as usize)
        .map_err(|_| invalid("corpus allocation refused"))?;
    bytes.resize(size as usize, 0);
    r.read_exact(&mut bytes)?;
    Ok((key[0] == 1, bytes))
}
#[derive(Default)]
struct Counts {
    originals: u64,
    dropped: u64,
    duplicates: u64,
    repairs: u64,
    encoded_bytes: u64,
}
fn flush_reorder(
    receiver: &mut ReceivePipeline,
    held: &mut Vec<(PacketOffer, Vec<u8>)>,
    now: u64,
    counts: &mut Counts,
) -> Result<(), Box<dyn Error>> {
    while let Some((offer, bytes)) = held.pop() {
        receiver.receive(offer.channel(), &bytes, now)?;
        receiver.receive(offer.channel(), &bytes, now)?;
        counts.duplicates += 1;
    }
    Ok(())
}
fn transfer(
    sender: &mut SendCache,
    receiver: &mut ReceivePipeline,
    now: u64,
    final_frame: bool,
    counts: &mut Counts,
) -> Result<(), Box<dyn Error>> {
    let mut out = [0; PACKET_BYTES];
    // This test impairment queue holds at most four packet-sized buffers.
    let mut held = Vec::with_capacity(4);
    while let Some(offer) = sender.next_packet(now, &mut out)? {
        counts.originals += 1;
        if offer.channel() == Channel::Video {
            if final_frame || counts.originals.is_multiple_of(5) {
                counts.dropped += 1;
                continue;
            }
            let bytes = out[..offer.byte_len()].to_vec();
            held.push((offer, bytes));
            if held.len() == 4 {
                flush_reorder(receiver, &mut held, now, counts)?;
            }
        } else {
            receiver.receive(offer.channel(), &out[..offer.byte_len()], now)?;
        }
    }
    flush_reorder(receiver, &mut held, now, counts)?;
    if let Some(n) = receiver.repair_request(now + 20_000, &mut out)? {
        sender.queue_repair(&out[..n], now + 20_000)?;
        while let Some(offer) = sender.next_repair_packet(now + 30_000, &mut out)? {
            receiver.receive(offer.channel(), &out[..offer.byte_len()], now + 30_000)?;
            counts.repairs += 1;
        }
    }
    Ok(())
}
fn arguments() -> io::Result<(std::ffi::OsString, std::ffi::OsString)> {
    let mut args = std::env::args_os().skip(1);
    let input = args
        .next()
        .ok_or_else(|| invalid("usage: hevc_delivery INPUT OUTPUT"))?;
    let output = args
        .next()
        .ok_or_else(|| invalid("usage: hevc_delivery INPUT OUTPUT"))?;
    if args.next().is_some() {
        return Err(invalid("unexpected argument"));
    }
    Ok((input, output))
}
fn run() -> Result<(), Box<dyn Error>> {
    let (input, output) = arguments()?;
    let source = File::open(input)?;
    if source.metadata()?.len() > MAX_CORPUS_BYTES {
        return Err(invalid("input file exceeds limit").into());
    }
    let mut input = BufReader::new(source);
    let frames = header(&mut input)?;
    let mut output = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output)?,
    );
    output.write_all(MAGIC)?;
    output.write_all(&frames.to_be_bytes())?;
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, PACKET_BYTES, 16_384, 64)?;
    let bindings = MediaBindings::new(1, 2, 3, 4)?;
    let epoch = MediaEpoch {
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    };
    let mut sender = SendCache::new(limits, bindings, epoch, SendPolicy::default())?;
    let mut receiver = ReceivePipeline::new(
        ReceiveConfig {
            limits,
            bindings,
            epoch,
            policy: ReceivePolicy::default(),
        },
        MediaBudget::new(limits.protocol())?,
    )?;
    receiver.decoder_configured(0)?;
    let mut counts = Counts::default();
    for frame in 0..frames {
        let (idr, bytes) = picture(&mut input, &mut counts.encoded_bytes)?;
        if frame == 0 && !idr {
            return Err(invalid("corpus must begin with an IDR").into());
        }
        let now = u64::from(frame) * 50_000;
        let progress = Progress {
            descriptor: FrameDescriptor {
                frame: u64::from(frame),
                total_bytes: u32::try_from(bytes.len())?,
                stride: limits.fragment_stride(),
                capture_micros: now,
                reference: if idr {
                    None
                } else {
                    Some(u64::from(frame) - 1)
                },
            },
            observed_micros: now,
            observation: SourceObservation::Unknown,
            pipeline: PipelineState::Running,
        };
        sender.push(
            progress,
            bytes.clone(),
            if frame == 0 {
                DeliveryMode::Recovery
            } else {
                DeliveryMode::Datagrams
            },
            now,
        )?;
        transfer(
            &mut sender,
            &mut receiver,
            now,
            frame + 1 == frames,
            &mut counts,
        )?;
        let assembled = receiver
            .take_decodable(now + 31_000)?
            .ok_or_else(|| invalid("repair failed to produce complete picture"))?;
        if assembled.bytes() != bytes || assembled.descriptor().frame != u64::from(frame) {
            return Err(invalid("picture corruption or order mismatch").into());
        }
        output.write_all(&[u8::from(idr)])?;
        output.write_all(&u32::try_from(assembled.bytes().len())?.to_be_bytes())?;
        output.write_all(assembled.bytes())?;
        // This offline diagnostic models decode completion ONLY after exact
        // byte comparison. The Python lane independently decodes the complete
        // delivered stream; this call alone is not codec/hardware evidence.
        receiver.acknowledge_decode(&assembled, true, now + 31_000)?;
        drop(assembled);
        if receiver.budget_usage().pictures != 0 {
            return Err(invalid("receiver retained picture after release").into());
        }
    }
    let mut extra = [0];
    if input.read(&mut extra)? != 0 {
        return Err(invalid("trailing corpus bytes").into());
    }
    output.flush()?;
    sender.close();
    receiver.close();
    println!(
        "{{\"frames\":{frames},\"encoded_bytes\":{},\"original_packets\":{},\"dropped_packets\":{},\"duplicate_packets\":{},\"repair_packets\":{},\"mode\":\"offline_delivery_not_live_transport\"}}",
        counts.encoded_bytes, counts.originals, counts.dropped, counts.duplicates, counts.repairs
    );
    Ok(())
}
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("HEVC delivery diagnostic failed: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corpus_header_refuses_zero_excess_and_truncation() {
        for count in [0_u32, MAX_FRAMES + 1] {
            let bytes = [MAGIC.as_slice(), &count.to_be_bytes()].concat();
            assert!(header(&mut bytes.as_slice()).is_err());
        }
        assert!(header(&mut b"FRH".as_slice()).is_err());
    }
    #[test]
    fn corpus_picture_refuses_oversize_before_allocation_and_truncated_body() {
        let bytes = [vec![1], u32::MAX.to_be_bytes().to_vec()].concat();
        assert!(picture(&mut bytes.as_slice(), &mut 0).is_err());
        let bytes = [vec![1], 4_u32.to_be_bytes().to_vec(), vec![1, 2]].concat();
        assert!(picture(&mut bytes.as_slice(), &mut 0).is_err());
        let bytes = [vec![2], 1_u32.to_be_bytes().to_vec(), vec![0]].concat();
        assert!(picture(&mut bytes.as_slice(), &mut 0).is_err());
    }
}
