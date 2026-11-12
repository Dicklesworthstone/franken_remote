//! Original macOS host playback source and real Opus encoding. This private
//! role owns no video, input, microphone or remote connection.
use fr_core::{
    audio::{AudioDirection, AudioStreamConfig, DEFAULT_JITTER_TARGET_MS},
    limits::ProtocolLimits,
};
use fr_media::{
    audio::{AudioEncoder, AudioPcmFrame},
    worker::{
        self, Error, Kind, Record, Sequence,
        audio::{Capture, Source},
    },
};
use fr_native::{
    macos_audio_capture::{Error as NativeError, Source as NativeSource},
    opus::{CodecLimits, Encoder},
};
use std::io;

fn typed(error: NativeError) -> Error {
    match error {
        NativeError::Configuration => Error::Malformed,
        NativeError::Allocation => Error::Allocation,
        NativeError::Unavailable
        | NativeError::Permission
        | NativeError::Expired
        | NativeError::Suspended
        | NativeError::Busy
        | NativeError::Format
        | NativeError::Closed => Error::Unsupported,
        NativeError::Native
        | NativeError::Clock
        | NativeError::BufferLimit
        | NativeError::Discontinuity
        | NativeError::Cleanup => Error::NativeFailure,
    }
}

fn refuse(
    output: &mut impl io::Write,
    identity: worker::Identity,
    error: Error,
) -> Result<(), Error> {
    let limits = ProtocolLimits::ABSOLUTE;
    Record::new(
        Kind::Refused,
        identity,
        (error as u16).to_be_bytes().to_vec(),
        &limits,
    )?
    .write(output, &limits)?;
    Err(error)
}

fn start(body: &[u8]) -> Result<(Capture, Encoder, NativeSource), Error> {
    let capture = Capture::decode(body)?;
    if capture.source != Source::MacSystem {
        return Err(Error::Unsupported);
    }
    let config = AudioStreamConfig::new(
        AudioDirection::Downlink,
        capture.generation,
        capture.channels,
        capture.frame_duration_ms,
        DEFAULT_JITTER_TARGET_MS,
    )
    .map_err(|_| Error::Malformed)?;
    let limits = CodecLimits::new(
        usize::from(capture.max_packet_bytes),
        config.expected_samples_per_frame(),
    )
    .map_err(|_| Error::Malformed)?;
    let mut encoder =
        Encoder::with_limits(limits, capture.bitrate, 5).map_err(|_| Error::NativeFailure)?;
    encoder
        .configure(config)
        .map_err(|_| Error::NativeFailure)?;
    let source = NativeSource::open(capture.channels, capture.frame_samples()).map_err(typed)?;
    Ok((capture, encoder, source))
}

fn fresh(result: Result<(), NativeError>, lost: &mut u32) -> Result<bool, Error> {
    match result {
        Ok(()) => Ok(true),
        Err(NativeError::Expired) => {
            *lost = lost.saturating_add(1);
            Ok(false)
        }
        Err(error) => Err(typed(error)),
    }
}

fn pull(
    source: &mut NativeSource,
    encoder: &mut Encoder,
    capture: &Capture,
    lost: &mut u32,
    identity: worker::Identity,
    output: &mut impl io::Write,
) -> Result<(), Error> {
    let batch = source.pull().map_err(typed)?;
    if batch.frames.len() > fr_native::host_audio_batch::MAX_FRAMES {
        return Err(Error::ResourceLimit);
    }
    let mut packets = Vec::with_capacity(batch.frames.len());
    for (index, frame) in batch.frames.iter().enumerate() {
        if !fresh(frame.check(), lost)? {
            continue;
        }
        let pcm = AudioPcmFrame::from_interleaved(
            capture.generation,
            capture.channels,
            frame.timestamp_samples(),
            frame.samples(),
        )
        .map_err(|_| Error::Malformed)?;
        // Native source identity and the immutable frame deadline bracket the
        // actual codec call. An expired result is discarded, never timestamped anew.
        encoder.submit_pcm(&pcm).map_err(|_| Error::NativeFailure)?;
        let packet = encoder
            .poll_packet()
            .map_err(|_| Error::NativeFailure)?
            .ok_or(Error::NativeFailure)?;
        if fresh(frame.check(), lost)? {
            packets.push((index, packet));
        }
    }
    let mut counters = batch.counters;
    counters.gaps = counters.gaps.saturating_add(*lost);
    // Keep every original Frame alive through serialization and final admission.
    let discarded =
        fr_native::host_audio_batch::publish(capture, identity, packets, counters, output, || {
            source.check_frames(&batch.frames).map_err(typed)
        })?;
    *lost = lost.saturating_add(discarded);
    Ok(())
}

pub(crate) fn serve(
    first: Record,
    mut sequence: Sequence,
    input: &mut impl io::Read,
    output: &mut impl io::Write,
) -> Result<(), Error> {
    let limits = ProtocolLimits::ABSOLUTE;
    let mut identity = first.header.identity;
    if first.header.kind != Kind::ConfigureAudio {
        return refuse(output, identity, Error::WrongState);
    }
    let (capture, mut encoder, mut source) = match start(first.body()) {
        Ok(started) => started,
        Err(error) => return refuse(output, identity, error),
    };
    let mut lost = 0;
    let result = (|| {
        Record::new(Kind::AudioReady, identity, first.into_body(), &limits)?
            .write(output, &limits)?;
        while let Some(request) = Record::read(input, &limits)? {
            identity = request.header.identity;
            sequence.accept(request.header)?;
            match request.header.kind {
                Kind::Stop => return Ok(true),
                Kind::ReadAudio => {
                    pull(
                        &mut source,
                        &mut encoder,
                        &capture,
                        &mut lost,
                        identity,
                        output,
                    )?;
                }
                _ => return Err(Error::WrongState),
            }
        }
        Ok(false)
    })();
    encoder.close();
    let closed = source.close().map_err(typed);
    match (result, closed) {
        (Ok(true), Ok(())) => {
            Record::new(Kind::Stopped, identity, Vec::new(), &limits)?.write(output, &limits)
        }
        (Ok(false), Ok(())) => Ok(()),
        (Err(error), _) | (_, Err(error)) => refuse(output, identity, error),
    }
}
