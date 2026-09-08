use super::*;
use fr_core::ids::CodecConfigurationGeneration;
use fr_media::config::{CodedGeometry, GopPolicy};

fn configuration(width: u32) -> CodecConfiguration {
    CodecConfiguration::new_baseline(
        CodecConfigurationGeneration::INITIAL,
        CodedGeometry::new(&ProtocolLimits::ABSOLUTE, width, 64, width, 64, 2).unwrap(),
        ColorInfo::sdr_bt709(),
        GopPolicy::baseline_for_frame_rate(30).unwrap(),
    )
    .unwrap()
}
fn encoded(width: u32, count: u64) -> Vec<EncodedAccessUnit> {
    let limits = ProtocolLimits::ABSOLUTE;
    let mut encoder = HevcEncoder::new(
        configuration(width),
        limits,
        EncodeBackend::SoftwareExplicit,
        30,
        1_000_000,
    )
    .unwrap();
    (0..count)
        .map(|n| {
            let source = BgraFrame::new(
                width,
                64,
                vec![u8::try_from(n * 20).unwrap(); usize::try_from(width).unwrap() * 64 * 4],
                &limits,
            )
            .unwrap();
            encoder
                .submit(&source, FrameId::from_raw(n), n * 33_333, n == 2)
                .unwrap();
            encoder.poll_output().unwrap()
        })
        .collect()
}
fn replace(unit: &EncodedAccessUnit, bytes: Vec<u8>) -> EncodedAccessUnit {
    EncodedAccessUnit::new(
        &ProtocolLimits::ABSOLUTE,
        unit.frame(),
        unit.kind(),
        unit.config_generation(),
        0,
        bytes,
    )
    .unwrap()
}

#[test]
fn wrong_sps_geometry_and_missing_sets_refuse_before_foreign_state_changes() {
    let limits = ProtocolLimits::ABSOLUTE;
    let valid = encoded(64, 1).pop().unwrap();
    let larger = encoded(128, 1).pop().unwrap();
    let mut decoder = HevcDecoder::new(configuration(64), limits).unwrap();
    assert_eq!(
        decoder.submit(&larger),
        Err(NativeError::UnsupportedBitstream)
    );
    let no_sets = replace(&valid, vec![0, 0, 0, 1, 0x28, 1, 0xad, 0xe0, 0x80]);
    assert_eq!(
        decoder.submit(&no_sets),
        Err(NativeError::UnsupportedBitstream)
    );
    assert!(decoder.pending.is_empty());
    assert_eq!(decoder.last, None);
    assert!(
        !decoder.closed,
        "preflight refusal need not poison an untouched decoder"
    );
    decoder.submit(&valid).unwrap();
    assert_eq!(decoder.poll_output().unwrap().0, valid.frame());
}

#[test]
fn changed_recovery_parameters_do_not_replace_the_live_configuration() {
    let units = encoded(64, 4);
    let mut decoder = HevcDecoder::new(configuration(64), ProtocolLimits::ABSOLUTE).unwrap();
    for unit in &units[..2] {
        decoder.submit(unit).unwrap();
        assert_eq!(decoder.poll_output().unwrap().0, unit.frame());
    }
    let unit = &units[2];
    assert!(unit.is_idr());
    let mut bytes = unit.bytes().to_vec();
    let pps = bytes
        .windows(5)
        .position(|w| w == [0, 0, 1, 0x44, 1])
        .unwrap();
    bytes[pps + 5] ^= 1;
    let changed = replace(unit, bytes);
    assert_eq!(
        decoder.submit(&changed),
        Err(NativeError::UnsupportedBitstream)
    );
    assert_eq!(decoder.last, Some(units[1].frame()));
    assert!(decoder.pending.is_empty());
    for unit in &units[2..] {
        decoder.submit(unit).unwrap();
        assert_eq!(decoder.poll_output().unwrap().0, unit.frame());
    }
}

#[test]
fn actual_decoder_backpressure_retries_do_not_consume_admission() {
    let units = encoded(64, 8);
    let mut decoder = HevcDecoder::new(configuration(64), ProtocolLimits::ABSOLUTE).unwrap();
    let mut received = 0;
    let mut retries = 0;
    for unit in &units {
        loop {
            match decoder.submit(unit) {
                Ok(()) => break,
                Err(NativeError::NeedDrain) => {
                    assert!(decoder.last.is_none_or(|last| last < unit.frame()));
                    let (id, _) = decoder.poll_output().unwrap();
                    assert_eq!(id, FrameId::from_raw(received));
                    received += 1;
                    retries += 1;
                }
                result => panic!("unexpected submit outcome: {result:?}"),
            }
        }
    }
    while received < u64::try_from(units.len()).unwrap() {
        let (id, _) = decoder.poll_output().unwrap();
        assert_eq!(id, FrameId::from_raw(received));
        received += 1;
    }
    assert!(
        retries > 0,
        "test must actually exercise native backpressure"
    );
    assert!(decoder.pending.is_empty());
}
