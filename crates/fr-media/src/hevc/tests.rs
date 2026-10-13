use super::*;
use crate::config::{CodedGeometry, ColorInfo, GopPolicy};
use fr_core::ids::CodecConfigurationGeneration;

// Real FFmpeg 7.1.5/libx265 4.1 Main8/420 BT.709, 320x240, ultrafast,
// zerolatency, ref=1, bframes=0. Slice fixtures contain only a header and a
// CABAC prefix: admission is not a successful-decode assertion.
const VPS: &str = "40010c01ffff01600000030090000003000003003cba0240";
const UNSPECIFIED_COLOR_SPS: &str =
    "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a040402080000003008000000f04";
const SPS: &str =
    "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04";
const PPS: &str = "4401c0718112";
const IDR: &str = "2801ade06702f86753c11ead2f1f6a69";
const P1: &str = "0201d009788230da9e2ce74eaffe0295";
const P2: &str = "0201d011fe2084daa7fdfe993afc6696";
fn hex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u8::from_str_radix(core::str::from_utf8(b).unwrap(), 16).unwrap())
        .collect()
}
fn au(nals: &[&[u8]]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for nal in nals {
        bytes.extend_from_slice(&[0, 0, 0, 1]);
        bytes.extend_from_slice(nal);
    }
    bytes
}
fn config(width: u32, height: u32) -> CodecConfiguration {
    CodecConfiguration::new_baseline(
        CodecConfigurationGeneration::INITIAL,
        CodedGeometry::new(&ProtocolLimits::ABSOLUTE, width, height, width, height, 2).unwrap(),
        ColorInfo::sdr_bt709(),
        GopPolicy::baseline_for_frame_rate(30).unwrap(),
    )
    .unwrap()
}
fn guard() -> HevcGuard {
    HevcGuard::new(config(320, 240), ProtocolLimits::ABSOLUTE, 4).unwrap()
}
fn startup() -> Vec<u8> {
    au(&[&hex(VPS), &hex(SPS), &hex(PPS), &hex(IDR)])
}

#[test]
fn actual_encoder_headers_establish_idr_then_one_previous_picture() {
    let mut g = guard();
    let info = g.validate_annex_b(&startup(), true).unwrap();
    assert!(info.idr);
    assert_eq!(info.decoded_pictures, 3);
    assert_eq!(info.level_idc, 60);
    assert_eq!(
        g.validate_annex_b(&au(&[&hex(P1)]), false).unwrap().poc_lsb,
        1
    );
    assert_eq!(
        g.validate_annex_b(&au(&[&hex(P2)]), false).unwrap().poc_lsb,
        2
    );
    assert!(g.validate_annex_b(&startup(), true).unwrap().idr);
    assert_eq!(
        g.validate_annex_b(&au(&[&hex(P1)]), false).unwrap().poc_lsb,
        1
    );
}
#[test]
fn early_resource_and_color_checks_reject_false_declarations() {
    let bytes = startup();
    let mut small = HevcGuard::new(config(320, 240), ProtocolLimits::ABSOLUTE, 2).unwrap();
    assert_eq!(
        small.validate_annex_b(&bytes, true),
        Err(HevcError::DpbLimit)
    );
    let mut wrong = HevcGuard::new(config(640, 240), ProtocolLimits::ABSOLUTE, 4).unwrap();
    assert_eq!(
        wrong.validate_annex_b(&bytes, true),
        Err(HevcError::GeometryMismatch)
    );
    assert_eq!(
        guard().validate_annex_b(
            &au(&[&hex(VPS), &hex(UNSPECIFIED_COLOR_SPS), &hex(PPS), &hex(IDR)]),
            true
        ),
        Err(HevcError::ColorMismatch)
    );
    let c = config(320, 240);
    let c = CodecConfiguration::new_baseline(
        c.generation(),
        c.geometry(),
        ColorInfo {
            range: crate::config::ColorRange::Full,
            ..ColorInfo::sdr_bt709()
        },
        c.gop(),
    )
    .unwrap();
    let mut wrong_range = HevcGuard::new(c, ProtocolLimits::ABSOLUTE, 4).unwrap();
    assert_eq!(
        wrong_range.validate_annex_b(&bytes, true),
        Err(HevcError::ColorMismatch)
    );
}
#[test]
fn failed_au_never_installs_parameters_or_advances_poc() {
    let mut g = guard();
    let bad = au(&[&hex(VPS), &hex(SPS), &hex(PPS), &hex(P1)]);
    assert!(g.validate_annex_b(&bad, true).is_err());
    assert!(g.sets.is_none());
    assert_eq!(g.last_poc, None);
    g.validate_annex_b(&startup(), true).unwrap();
    assert_eq!(
        g.validate_annex_b(&au(&[&hex(P2)]), false),
        Err(HevcError::ReferenceMismatch)
    );
    assert_eq!(g.last_poc, Some(0));
    g.validate_annex_b(&au(&[&hex(P1)]), false).unwrap();
}
#[test]
fn native_backpressure_can_discard_a_candidate_without_consuming_it() {
    let mut original = guard();
    original.validate_annex_b(&startup(), true).unwrap();
    let mut candidate = original.clone();
    candidate.validate_annex_b(&au(&[&hex(P1)]), false).unwrap();
    assert_eq!(original.last_poc, Some(0));
    assert!(Arc::ptr_eq(
        original.sets.as_ref().unwrap(),
        candidate.sets.as_ref().unwrap()
    ));
    original.validate_annex_b(&au(&[&hex(P1)]), false).unwrap();
}
#[test]
fn freeze_all_parameter_bytes_even_across_recovery_idrs() {
    let mut g = guard();
    g.validate_annex_b(&startup(), true).unwrap();
    let mut pps = hex(PPS);
    pps[3] ^= 1;
    assert_eq!(
        g.validate_annex_b(&au(&[&hex(VPS), &hex(SPS), &pps, &hex(IDR)]), true),
        Err(HevcError::ChangedParameterSet)
    );
    assert_eq!(
        g.validate_annex_b(&au(&[&hex(IDR)]), true),
        Err(HevcError::MissingParameterSet)
    );
    assert_eq!(
        g.validate_annex_b(&au(&[&hex(PPS), &hex(P1)]), false),
        Err(HevcError::ChangedParameterSet)
    );
    g.validate_annex_b(&au(&[&hex(P1)]), false).unwrap();
}
#[test]
fn reject_cra_b_slices_multiple_pictures_and_nonzero_layers() {
    assert!(guard().validate_annex_b(&au(&[&hex(P1)]), false).is_err());
    for kind in [0, 2, 16, 18, 21, 22, 36, 37, 38, 41, 63] {
        let mut nal = hex(IDR);
        nal[0] = kind << 1;
        assert!(
            guard()
                .validate_annex_b(&au(&[&hex(VPS), &hex(SPS), &hex(PPS), &nal]), true)
                .is_err()
        );
    }
    for header in [[0xa8, 1], [0x29, 1], [0x28, 0], [0x28, 2], [0x28, 9]] {
        let mut nal = hex(IDR);
        nal[..2].copy_from_slice(&header);
        assert!(
            guard()
                .validate_annex_b(&au(&[&hex(VPS), &hex(SPS), &hex(PPS), &nal]), true)
                .is_err()
        );
    }
    let mut nal = hex(IDR);
    nal[2] = 0xe0; // first slice, no-output=1, pps=0, B slice type=0.
    assert!(
        guard()
            .validate_annex_b(&au(&[&hex(VPS), &hex(SPS), &hex(PPS), &nal]), true)
            .is_err()
    );
    assert!(
        guard()
            .validate_annex_b(
                &au(&[&hex(VPS), &hex(SPS), &hex(PPS), &hex(IDR), &hex(IDR)]),
                true
            )
            .is_err()
    );
}
#[test]
fn every_parameter_truncation_and_oversize_is_rejected() {
    for (position, original) in [VPS, SPS, PPS].iter().enumerate() {
        let data = hex(original);
        for len in 0..data.len() {
            let mut sets = [hex(VPS), hex(SPS), hex(PPS)];
            sets[position] = data[..len].to_vec();
            assert!(
                guard()
                    .validate_annex_b(&au(&[&sets[0], &sets[1], &sets[2], &hex(IDR)]), true)
                    .is_err(),
                "parameter {position} truncation {len}"
            );
        }
    }
    let mut huge = vec![0x55; 4097];
    huge[0] = 0x40;
    huge[1] = 1;
    assert_eq!(
        guard().validate_annex_b(&au(&[&huge, &hex(SPS), &hex(PPS), &hex(IDR)]), true),
        Err(HevcError::Limit)
    );
}
#[test]
fn bounded_golomb_and_emulation_prevention_are_fail_closed() {
    let mut bits = nal::Bits::new(&[0xff]);
    assert_eq!(bits.ue(0), Ok(0));
    let mut bits = nal::Bits::new(&[0, 0, 3, 0, 0, 3, 0, 0]);
    assert!(bits.ue(u32::MAX - 1).is_err());
    for bytes in [&[0, 0, 3][..], &[0, 0, 3, 4], &[0, 0, 2], &[0, 0, 0]] {
        let mut bits = nal::Bits::new(bytes);
        assert!(bits.read(32).is_err());
    }
    let mut bits = nal::Bits::new(&[0, 0, 3, 1]);
    assert_eq!(bits.read(24), Ok(1));
}
#[test]
fn malformed_framing_and_extra_parameter_sets_do_not_panic() {
    for bytes in [
        &[][..],
        &[0],
        &[0, 0, 1],
        &[0, 0, 1, 0],
        &[1, 0, 0, 1, 0x28, 1, 0x80],
    ] {
        assert!(guard().validate_annex_b(bytes, true).is_err());
    }
    let mut bytes = startup();
    bytes.extend_from_slice(&[0, 0, 1]);
    assert!(guard().validate_annex_b(&bytes, true).is_err());
    let mut g = guard();
    assert!(
        g.validate_annex_b(
            &au(&[&hex(VPS), &hex(VPS), &hex(SPS), &hex(PPS), &hex(IDR)]),
            true
        )
        .is_err()
    );
    // Mutate every byte and all 8 bits. This tests panic safety, not that every
    // changed legal stream must fail (some flags are permitted to vary).
    let original = startup();
    for i in 0..original.len() {
        for bit in 0..8 {
            let mut bytes = original.clone();
            bytes[i] ^= 1 << bit;
            let _ = guard().validate_annex_b(&bytes, true);
        }
    }
}
#[test]
fn debug_output_contains_no_parameter_or_screen_content() {
    let mut g = guard();
    g.validate_annex_b(&startup(), true).unwrap();
    let debug = format!("{g:?}");
    assert!(!debug.contains("bytes"));
    assert!(!debug.contains(VPS));
    assert!(!debug.contains("4001"));
    assert!(!debug.contains("[64, 1"));
}

#[test]
fn exact_configuration_admits_parameters_without_inventing_decoded_history() {
    let mut encoder = guard();
    encoder.validate_annex_b(&startup(), true).unwrap();
    let record = encoder.decoder_record().unwrap();
    let mut decoder = HevcGuard::from_decoder_record(
        config(320, 240),
        ProtocolLimits::ABSOLUTE,
        4,
        record.bytes(),
    )
    .unwrap();
    assert_eq!(decoder.decoder_record().unwrap().bytes(), record.bytes());
    assert_eq!(decoder.last_poc, None);
    assert_eq!(decoder.available_history, 0);
    assert!(decoder.validate_annex_b(&au(&[&hex(P1)]), false).is_err());
    assert_eq!(decoder.last_poc, None);
    decoder.validate_annex_b(&startup(), true).unwrap();
    decoder.validate_annex_b(&au(&[&hex(P1)]), false).unwrap();
    assert_eq!(decoder.last_poc, Some(1));
}

#[test]
fn decoder_record_rejects_truncation_header_disagreement_and_resource_demands() {
    let mut encoder = guard();
    encoder.validate_annex_b(&startup(), true).unwrap();
    let record = encoder.decoder_record().unwrap();
    let admit = |bytes: &[u8]| {
        HevcGuard::from_decoder_record(config(320, 240), ProtocolLimits::ABSOLUTE, 4, bytes)
    };
    for len in 0..record.bytes().len() {
        assert!(admit(&record.bytes()[..len]).is_err(), "truncation {len}");
    }
    for byte in 0..23 {
        for bit in 0..8 {
            let mut changed = record.bytes().to_vec();
            changed[byte] ^= 1 << bit;
            assert!(admit(&changed).is_err(), "header byte {byte} bit {bit}");
        }
    }
    for byte in [23, 24, 25, 28] {
        let mut changed = record.bytes().to_vec();
        changed[byte] ^= 2;
        assert!(admit(&changed).is_err(), "array count/type byte {byte}");
    }
    let mut changed = record.bytes().to_vec();
    changed[26..28].copy_from_slice(&4097_u16.to_be_bytes());
    assert_eq!(admit(&changed).unwrap_err(), HevcError::Limit);
    let mut trailing = record.bytes().to_vec();
    trailing.push(0);
    assert_eq!(admit(&trailing).unwrap_err(), HevcError::Framing);
    assert_eq!(
        admit(&vec![0; MAX_DECODER_RECORD_BYTES + 1]).unwrap_err(),
        HevcError::Limit
    );
    assert_eq!(
        HevcGuard::from_decoder_record(
            config(640, 240),
            ProtocolLimits::ABSOLUTE,
            4,
            record.bytes()
        )
        .unwrap_err(),
        HevcError::GeometryMismatch
    );
    assert_eq!(
        HevcGuard::from_decoder_record(
            config(320, 240),
            ProtocolLimits::ABSOLUTE,
            2,
            record.bytes()
        )
        .unwrap_err(),
        HevcError::DpbLimit
    );
}

#[test]
fn private_decoder_startup_binds_the_record_generation_and_geometry() {
    let mut guard = guard();
    guard.validate_annex_b(&startup(), true).unwrap();
    let record = guard.decoder_record().unwrap();
    let configuration = crate::worker::Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: crate::worker::Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: 1024 * 1024,
        generation: record.generation(),
    };
    let body = configuration.encode_decoder(&record).unwrap();
    assert_eq!(
        crate::worker::Configuration::decode_decoder(&body).unwrap(),
        (configuration, record.bytes())
    );
    assert_eq!(
        crate::worker::Configuration {
            generation: CodecConfigurationGeneration::from_raw(1),
            ..configuration
        }
        .encode_decoder(&record),
        Err(crate::worker::Error::WrongEpoch)
    );
    assert_eq!(
        crate::worker::Configuration {
            width: 640,
            ..configuration
        }
        .encode_decoder(&record),
        Err(crate::worker::Error::Unsupported)
    );
}
/// Live defect (2026-10-12): NVENC writes a prefix `pic_timing` SEI (payload
/// type 1) on every picture, which admission rightly refuses, so the host
/// rejected its own encoder output. The host strips encoder SEI first; the
/// stripped unit is exactly the SEI-free unit and admission stays strict.
#[test]
fn encoder_sei_is_stripped_before_admission_without_widening_it() {
    // nal_unit_type 39 (prefix SEI) / 40 (suffix SEI), payload type 1, 4 bytes.
    let pic_timing = hex("4e01010412345678" /* type,size,payload */);
    let pic_timing = [pic_timing.as_slice(), &[0x80]].concat();
    let suffix = [hex("5001010412345678").as_slice(), &[0x80]].concat();
    let with_sei = au(&[
        &hex(VPS),
        &hex(SPS),
        &hex(PPS),
        &pic_timing,
        &hex(IDR),
        &suffix,
    ]);
    assert_eq!(
        guard().validate_annex_b(&with_sei, true).unwrap_err(),
        HevcError::UnsupportedSyntax
    );
    let stripped = strip_encoder_sei(&with_sei).unwrap();
    assert_eq!(stripped, startup());
    assert!(guard().validate_annex_b(&stripped, true).unwrap().idr);
    // Three-byte start codes grow by one byte per NAL; the up-front
    // reservation covers it, so no push reallocates.
    let short: Vec<u8> = [hex(VPS), hex(SPS), hex(PPS), hex(IDR)]
        .iter()
        .flat_map(|nal| [&[0u8, 0, 1][..], nal].concat())
        .collect();
    let widened = strip_encoder_sei(&short).unwrap();
    assert_eq!(widened, startup());
    assert_eq!(widened.len(), short.len() + 4);
    assert!(widened.capacity() >= short.len() + nal::MAX_NALS);
    // Units without SEI pass through unchanged; broken framing is refused.
    assert_eq!(strip_encoder_sei(&startup()).unwrap(), startup());
    assert_eq!(strip_encoder_sei(&[]).unwrap_err(), HevcError::Framing);
    assert_eq!(
        strip_encoder_sei(&[1, 2, 3]).unwrap_err(),
        HevcError::Framing
    );
}
// Real NVENC (GTX 1070, driver 580.159, FFmpeg 7.1.1 hevc_nvenc p1/ull, the
// bridge's options) 1920x1080 coded 1088, VUI colour rewritten to the BT.709
// the bridge's context requests. NVENC declares its one previous-picture
// reference as an SPS RPS table (slices select it) and writes VUI HRD timing.
const NVENC_VPS: &str = "40010c01ffff216000000300900000030000030078ac09";
const NVENC_SPS: &str = "420101216000000300900000030000030078a003c0801107cb96b4a42592e3016a02020208000003000800000300f3002ef288000249f000001e848020";
const NVENC_PPS: &str = "4401c0f7c0cc90";
const NVENC_IDR: &str =
    "2601af138079f0d1e39fdd01376c4ff3a7cc2cf2c9ffb1f304fc7221ba042ca3d97dc0df8e3e44f0";
const NVENC_P1: &str =
    "0201d00f22703e8b918ee40b27c43a487ffeac9ba67fffffff63806d0212f9ee381082bffffd8fea";
const NVENC_P2: &str =
    "0201d01722707cc6998788679f8791b313ef9bf97e8f51b4263db91b8b596a2d02f3c017e10df8d4";
/// Live defect (2026-10-12): `--encoder nvenc` output was refused (`Limit`)
/// because admission required inline slice RPS and no VUI HRD. SPS RPS sets
/// are admitted under the same rules as inline ones (past pictures only, the
/// first being the previous picture) and HRD is skipped field by field.
#[test]
fn real_nvenc_sps_rps_tables_and_vui_hrd_are_admitted_under_the_same_rules() {
    let config = CodecConfiguration::new_baseline(
        CodecConfigurationGeneration::INITIAL,
        CodedGeometry::from_visible(&ProtocolLimits::ABSOLUTE, 1920, 1080, 16).unwrap(),
        ColorInfo::sdr_bt709(),
        GopPolicy::baseline_for_frame_rate(30).unwrap(),
    )
    .unwrap();
    let mut g = HevcGuard::new(config, ProtocolLimits::ABSOLUTE, 4).unwrap();
    let idr = au(&[
        &hex(NVENC_VPS),
        &hex(NVENC_SPS),
        &hex(NVENC_PPS),
        &hex(NVENC_IDR),
    ]);
    assert!(g.validate_annex_b(&idr, true).unwrap().idr);
    assert_eq!(
        g.validate_annex_b(&au(&[&hex(NVENC_P1)]), false)
            .unwrap()
            .poc_lsb,
        1
    );
    assert_eq!(
        g.validate_annex_b(&au(&[&hex(NVENC_P2)]), false)
            .unwrap()
            .poc_lsb,
        2
    );
    // A P picture before any IDR still has no reference to select.
    let mut cold = HevcGuard::new(config, ProtocolLimits::ABSOLUTE, 4).unwrap();
    assert!(
        cold.validate_annex_b(&au(&[&hex(NVENC_P1)]), false)
            .is_err()
    );
    // Out-of-order POC is still a reference mismatch with SPS-selected sets.
    let mut skipped = HevcGuard::new(config, ProtocolLimits::ABSOLUTE, 4).unwrap();
    assert!(skipped.validate_annex_b(&idr, true).is_ok());
    assert_eq!(
        skipped
            .validate_annex_b(&au(&[&hex(NVENC_P2)]), false)
            .unwrap_err(),
        HevcError::ReferenceMismatch
    );
}
