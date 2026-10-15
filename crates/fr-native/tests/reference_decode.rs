#![cfg(all(target_os = "linux", feature = "linux-media"))]
//! Actual software HEVC encode/decode, with selected outputs used only as
//! references. No X11, transport, optical visibility or hardware qualification.
use fr_core::{ids::CodecConfigurationGeneration, limits::ProtocolLimits};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId},
    hevc::HevcGuard,
    worker::{Backend, Configuration},
};
use fr_native::{BgraFrame, EncodeBackend, HevcDecoder, HevcEncoder, NativeError};

#[path = "reference_decode/allocations.rs"]
mod allocations;

fn configuration() -> Configuration {
    Configuration {
        width: 192,
        height: 128,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 1_000_000,
        max_access_unit_bytes: 1_048_576,
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
fn encoder() -> HevcEncoder {
    let c = configuration();
    HevcEncoder::new(
        c.codec().unwrap(),
        c.limits().unwrap(),
        EncodeBackend::SoftwareExplicit,
        u32::from(c.fps),
        c.bitrate,
    )
    .unwrap()
}
fn encode(encoder: &mut HevcEncoder, id: u64) -> EncodedAccessUnit {
    let c = configuration();
    let shift = u8::try_from(id % 256).unwrap();
    let bytes = (0..c.width * c.height)
        .flat_map(|index| {
            [
                u8::try_from(index % 256).unwrap().wrapping_add(shift),
                u8::try_from((index / c.width) % 256).unwrap(),
                shift.wrapping_mul(17),
                255,
            ]
        })
        .collect();
    let frame = BgraFrame::new(c.width, c.height, bytes, &c.limits().unwrap()).unwrap();
    encoder
        .submit(&frame, FrameId::from_raw(id), id * 33_333, id == 0)
        .unwrap();
    encoder.poll_output().unwrap()
}
fn decoder(first: &EncodedAccessUnit) -> HevcDecoder {
    let c = configuration();
    let limits = c.limits().unwrap();
    let mut guard = HevcGuard::new(c.codec().unwrap(), limits, 4).unwrap();
    guard.validate_length_prefixed(first.bytes(), true).unwrap();
    HevcDecoder::new(
        c.codec().unwrap(),
        limits,
        guard.decoder_record().unwrap().bytes(),
    )
    .unwrap()
}

#[test]
fn reference_only_decode_keeps_every_later_presented_picture_identical() {
    let mut encoder = encoder();
    let first = encode(&mut encoder, 0);
    let mut baseline = decoder(&first);
    let mut selected = decoder(&first);
    let mut first = Some(first);
    let mut predicted = 0;
    for n in 0..90 {
        let unit = first.take().unwrap_or_else(|| encode(&mut encoder, n));
        predicted += usize::from(!unit.is_idr());
        baseline.submit(&unit).unwrap();
        selected.submit(&unit).unwrap();
        let (expected_id, expected) = baseline.poll_output().unwrap();
        // Includes discarding the initial IDR's output, long predicted runs,
        // and resuming presentation repeatedly on this same decoder instance.
        if n % 13 == 12 || n == 89 {
            let (id, pixels) = selected.poll_output().unwrap();
            assert_eq!(id, expected_id);
            assert_eq!(pixels.pixels(), expected.pixels());
            selected.recycle(pixels);
        } else {
            let (id, allocations) = allocations::during(|| selected.poll_reference());
            assert_eq!(id.unwrap(), expected_id);
            assert_eq!(
                allocations, 0,
                "reference-only output allocates no Rust pixel buffer"
            );
        }
        baseline.recycle(expected);
        assert_eq!(selected.poll_reference(), Err(NativeError::NeedInput));
    }
    assert!(
        predicted > 60,
        "fixture must exercise real codec dependencies"
    );
}

#[test]
fn empty_reference_polls_allocate_nothing_and_do_not_consume_the_next_frame() {
    let mut encoder = encoder();
    let first = encode(&mut encoder, 0);
    let mut decoder = decoder(&first);
    for _ in 0..32 {
        let (result, allocations) = allocations::during(|| decoder.poll_reference());
        assert_eq!(result, Err(NativeError::NeedInput));
        assert_eq!(allocations, 0);
    }
    decoder.submit(&first).unwrap();
    assert_eq!(decoder.poll_reference().unwrap(), first.frame());
    assert_eq!(decoder.poll_reference(), Err(NativeError::NeedInput));
    let next = encode(&mut encoder, 1);
    decoder.submit(&next).unwrap();
    assert_eq!(decoder.poll_output().unwrap().0, next.frame());
    assert_eq!(decoder.poll_reference(), Err(NativeError::NeedInput));
}

#[test]
fn reference_completion_cannot_mutate_a_retained_presented_buffer() {
    let mut encoder = encoder();
    let first = encode(&mut encoder, 0);
    let mut decoder = decoder(&first);
    decoder.submit(&first).unwrap();
    let (_, held) = decoder.poll_output().unwrap();
    let original = held.pixels().to_vec();
    for n in 1..24 {
        let unit = encode(&mut encoder, n);
        decoder.submit(&unit).unwrap();
        assert_eq!(decoder.poll_reference().unwrap(), unit.frame());
        assert_eq!(held.pixels(), original);
    }
    let final_unit = encode(&mut encoder, 24);
    decoder.submit(&final_unit).unwrap();
    let (id, final_pixels) = decoder.poll_output().unwrap();
    assert_eq!(id, final_unit.frame());
    assert_ne!(final_pixels.pixels(), original);
    assert_eq!(held.pixels(), original);
}

#[test]
fn reference_mode_does_not_bypass_bitstream_admission_or_consume_a_refused_input() {
    let mut encoder = encoder();
    let first = encode(&mut encoder, 0);
    let mut decoder = decoder(&first);
    decoder.submit(&first).unwrap();
    decoder.poll_reference().unwrap();
    let next = encode(&mut encoder, 1);
    let malformed = EncodedAccessUnit::new(
        &ProtocolLimits::ABSOLUTE,
        next.frame(),
        next.kind(),
        next.config_generation(),
        next.capture_micros(),
        next.bytes()[..2].to_vec(),
    )
    .unwrap();
    assert_eq!(
        decoder.submit(&malformed),
        Err(NativeError::UnsupportedBitstream)
    );
    assert_eq!(decoder.poll_reference(), Err(NativeError::NeedInput));
    decoder.submit(&next).unwrap();
    assert_eq!(decoder.poll_reference().unwrap(), next.frame());
    assert_eq!(decoder.submit(&next), Err(NativeError::StaleGeneration));
    assert_eq!(decoder.poll_reference(), Err(NativeError::NeedInput));
}
