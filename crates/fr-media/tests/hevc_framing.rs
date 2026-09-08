use fr_core::{
    ids::CodecConfigurationGeneration,
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_media::{
    config::{CodecConfiguration, CodedGeometry, ColorInfo, GopPolicy},
    hevc::{
        HevcError, HevcGuard,
        framing::{annex_b_to_length_prefixed, length_prefixed_to_annex_b},
    },
};
const VPS: &str = "40010c01ffff01600000030090000003000003003cba0240";
const SPS: &str =
    "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04";
const PPS: &str = "4401c0718112";
const IDR: &str = "2801ade06702f86753c11ead2f1f6a69";
const P1: &str = "0201d009788230da9e2ce74eaffe0295";
fn hex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u8::from_str_radix(core::str::from_utf8(b).unwrap(), 16).unwrap())
        .collect()
}
fn annex(nals: &[&str]) -> Vec<u8> {
    nals.iter()
        .flat_map(|n| [vec![0, 0, 0, 1], hex(n)].concat())
        .collect()
}
fn canonical(nals: &[&str]) -> Vec<u8> {
    annex_b_to_length_prefixed(&annex(nals), ProtocolLimits::ABSOLUTE).unwrap()
}
fn guard() -> HevcGuard {
    let limits = ProtocolLimits::ABSOLUTE;
    HevcGuard::new(
        CodecConfiguration::new_baseline(
            CodecConfigurationGeneration::INITIAL,
            CodedGeometry::new(&limits, 320, 240, 320, 240, 2).unwrap(),
            ColorInfo::sdr_bt709(),
            GopPolicy::baseline_for_frame_rate(30).unwrap(),
        )
        .unwrap(),
        limits,
        4,
    )
    .unwrap()
}
#[test]
fn canonical_roundtrip_and_both_admission_paths_agree() {
    let original = annex(&[VPS, SPS, PPS, IDR]);
    let wire = canonical(&[VPS, SPS, PPS, IDR]);
    assert_eq!(
        length_prefixed_to_annex_b(&wire, ProtocolLimits::ABSOLUTE).unwrap(),
        original
    );
    assert_eq!(
        guard().validate_annex_b(&original, true),
        guard().validate_length_prefixed(&wire, true)
    );
    assert!(guard().validate_length_prefixed(&original, true).is_err());
    assert!(guard().validate_annex_b(&wire, true).is_err());
}
#[test]
fn mixed_start_codes_and_annex_b_zero_padding_normalize_once() {
    let mut bytes = vec![0, 0];
    for (i, nal) in [VPS, SPS, PPS, IDR].iter().enumerate() {
        bytes.extend_from_slice(if i % 2 == 0 {
            &[0, 0, 1]
        } else {
            &[0, 0, 0, 1]
        });
        bytes.extend_from_slice(&hex(nal));
    }
    bytes.extend_from_slice(&[0, 0, 0]);
    assert_eq!(
        annex_b_to_length_prefixed(&bytes, ProtocolLimits::ABSOLUTE).unwrap(),
        canonical(&[VPS, SPS, PPS, IDR])
    );
}
#[test]
fn length_fields_are_checked_before_slicing_or_allocating() {
    for bytes in [
        &[][..],
        &[0],
        &[0, 0],
        &[0, 0, 0],
        &[0, 0, 0, 0],
        &[0, 0, 0, 1, 40],
        &[0, 0, 0, 2, 40, 1],
        &[255, 255, 255, 255, 40, 1, 128],
        &[0, 0, 0, 4, 40, 1, 128],
    ] {
        assert!(length_prefixed_to_annex_b(bytes, ProtocolLimits::ABSOLUTE).is_err());
    }
    let mut data = canonical(&[IDR]);
    data.push(0);
    assert!(length_prefixed_to_annex_b(&data, ProtocolLimits::ABSOLUTE).is_err());
}
#[test]
fn native_conversion_refuses_hidden_start_codes_even_after_slice_headers() {
    for suffix in [
        &[0, 0, 1, 0x42, 1][..],
        &[0, 0, 2],
        &[0, 0, 3],
        &[0, 0, 3, 4],
    ] {
        let mut nal = hex(IDR);
        nal.extend_from_slice(suffix);
        let mut bytes = u32::try_from(nal.len()).unwrap().to_be_bytes().to_vec();
        bytes.extend_from_slice(&nal);
        assert!(length_prefixed_to_annex_b(&bytes, ProtocolLimits::ABSOLUTE).is_err());
    }
}
#[test]
fn nal_count_and_expanded_output_have_independent_bounds() {
    let raw = [0, 0, 1, 40, 1, 128];
    let limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_encoded_access_unit_bytes: Some(6),
        ..LimitOverrides::default()
    })
    .unwrap();
    assert_eq!(
        annex_b_to_length_prefixed(&raw, limits),
        Err(HevcError::Limit)
    );
    assert!(annex_b_to_length_prefixed(&raw.repeat(256), ProtocolLimits::ABSOLUTE).is_ok());
    assert_eq!(
        annex_b_to_length_prefixed(&raw.repeat(257), ProtocolLimits::ABSOLUTE),
        Err(HevcError::Limit)
    );
}
#[test]
fn hvcc_and_codec_identifier_come_from_the_admitted_stream() {
    let mut g = guard();
    assert!(matches!(
        g.decoder_record(),
        Err(HevcError::MissingParameterSet)
    ));
    g.validate_length_prefixed(&canonical(&[VPS, SPS, PPS, IDR]), true)
        .unwrap();
    let record = g.decoder_record().unwrap();
    assert_eq!(record.codec(), "hvc1.1.6.L60.90");
    assert_eq!(record.generation(), CodecConfigurationGeneration::INITIAL);
    assert_eq!(
        &record.bytes()[..13],
        &[1, 1, 96, 0, 0, 0, 144, 0, 0, 0, 0, 0, 60]
    );
    assert_eq!(
        &record.bytes()[13..23],
        &[240, 0, 252, 253, 248, 248, 0, 0, 15, 3]
    );
    let mut offset = 23;
    for (kind, parameter) in [(32, VPS), (33, SPS), (34, PPS)] {
        assert_eq!(&record.bytes()[offset..offset + 3], &[128 | kind, 0, 1]);
        let length = usize::from(u16::from_be_bytes(
            record.bytes()[offset + 3..offset + 5].try_into().unwrap(),
        ));
        offset += 5;
        assert_eq!(&record.bytes()[offset..offset + length], hex(parameter));
        offset += length;
    }
    assert_eq!(offset, record.bytes().len());
    let diagnostic = format!("{record:?}");
    assert!(!diagnostic.contains(VPS));
    assert!(!diagnostic.contains("[64, 1"));
}
#[test]
fn browser_samples_remove_parameter_sets_and_recovery_repeats_exact_description() {
    let mut g = guard();
    let first = g
        .prepare_hvc1(&canonical(&[VPS, SPS, PPS, IDR]), true)
        .unwrap();
    assert!(first.picture().idr);
    assert_eq!(first.bytes(), canonical(&[IDR]));
    let description = first.configuration().unwrap().bytes().to_vec();
    assert!(g.prepare_hvc1(&canonical(&[P1]), true).is_err());
    let next = g.prepare_hvc1(&canonical(&[P1]), false).unwrap();
    assert!(!next.picture().idr);
    assert_eq!(next.picture().poc_lsb, 1);
    assert!(next.configuration().is_none());
    assert_eq!(next.bytes(), canonical(&[P1]));
    let recovery = g
        .prepare_hvc1(&canonical(&[VPS, SPS, PPS, IDR]), true)
        .unwrap();
    assert_eq!(recovery.configuration().unwrap().bytes(), description);
}
