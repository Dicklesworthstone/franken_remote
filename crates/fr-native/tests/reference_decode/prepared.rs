//! The prepared slice pool uses the same admission, ordering and pixel path
//! as the single-thread decoder. Process confinement is exercised by the real
//! worker tests and the native `decoder_threads.c` all-thread syscall probes.
use super::*;

fn prepared(first: &EncodedAccessUnit) -> HevcDecoder {
    let c = configuration();
    let limits = c.limits().unwrap();
    let mut guard = HevcGuard::new(c.codec().unwrap(), limits, 4).unwrap();
    guard.validate_length_prefixed(first.bytes(), true).unwrap();
    HevcDecoder::prepare(c.codec().unwrap(), limits)
        .unwrap()
        .configure(guard.decoder_record().unwrap().bytes())
        .unwrap()
}

#[test]
fn prepared_slice_decode_matches_single_thread_pixels_and_reference_history() {
    let mut encoder = encoder();
    let first = encode(&mut encoder, 0);
    let mut baseline = decoder(&first);
    let mut parallel = prepared(&first);
    // API configuration consumed no picture and invented no completion.
    assert_eq!(parallel.poll_reference(), Err(NativeError::NeedInput));
    let mut first = Some(first);
    let mut held = None;
    for n in 0..40 {
        let unit = first.take().unwrap_or_else(|| encode(&mut encoder, n));
        baseline.submit(&unit).unwrap();
        parallel.submit(&unit).unwrap();
        let (expected_id, expected) = baseline.poll_output().unwrap();
        if n % 5 == 0 || n == 39 {
            let (id, pixels) = parallel.poll_output().unwrap();
            assert_eq!(id, expected_id);
            assert_eq!(pixels.pixels(), expected.pixels());
            if n == 0 {
                held = Some((pixels.pixels().to_vec(), pixels));
            } else {
                parallel.recycle(pixels);
            }
        } else {
            let (id, allocations) = allocations::during(|| parallel.poll_reference());
            assert_eq!(id.unwrap(), expected_id);
            assert_eq!(allocations, 0);
        }
        if let Some((original, pixels)) = &held {
            assert_eq!(pixels.pixels(), original);
        }
        baseline.recycle(expected);
        assert_eq!(parallel.poll_reference(), Err(NativeError::NeedInput));
    }
}

#[test]
fn prepared_configuration_refuses_malformed_records_and_unused_pools_can_close() {
    let c = configuration();
    let limits = c.limits().unwrap();
    for record in [&[][..], &[0; 23][..], &[1, 2, 3][..]] {
        assert!(matches!(
            HevcDecoder::prepare(c.codec().unwrap(), limits)
                .unwrap()
                .configure(record),
            Err(NativeError::InvalidConfiguration)
        ));
    }
    for _ in 0..4 {
        drop(HevcDecoder::prepare(c.codec().unwrap(), limits).unwrap());
    }
}

#[test]
fn prepared_decode_preserves_generation_replay_and_bitstream_admission() {
    let mut encoder = encoder();
    let first = encode(&mut encoder, 0);
    let mut decoder = prepared(&first);
    decoder.submit(&first).unwrap();
    assert_eq!(decoder.poll_reference().unwrap(), first.frame());
    assert_eq!(decoder.submit(&first), Err(NativeError::StaleGeneration));
    let next = encode(&mut encoder, 1);
    let foreign = EncodedAccessUnit::new(
        &ProtocolLimits::ABSOLUTE,
        next.frame(),
        next.kind(),
        next.config_generation().next().unwrap(),
        next.capture_micros(),
        next.bytes().to_vec(),
    )
    .unwrap();
    assert_eq!(decoder.submit(&foreign), Err(NativeError::StaleGeneration));
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
    assert_eq!(decoder.poll_output().unwrap().0, next.frame());
}
