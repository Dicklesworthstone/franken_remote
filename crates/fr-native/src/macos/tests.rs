use super::*;
use fr_core::ids::{CodecConfigurationGeneration, RecoveryGeneration};
use fr_media::config::{CodedGeometry, ColorInfo, GopPolicy};
use fr_media::hevc::framing::annex_b_to_length_prefixed;
use fr_media::hevc::strip_encoder_sei;

// Real FFmpeg 8.1.2 / libx265 4.2 (build 216) Main8 4:2:0 BT.709 limited range,
// 128x96 testsrc2, three pictures (one IDR, two P), 100 kbit/s, encoded with
// the host's libx265 parameters from bridge.c (ultrafast, zerolatency,
// bframes=0, ref=1, aud=1, repeat-headers=1) plus explicit
// colorprim/transfer/colormatrix=bt709 and info=0. Annex B, as hex text.
const STREAM_HEX: &str = include_str!("tiny128.hex");
// The same encoder without colour parameters: an SPS that declares no colour
// description, which the BT.709 baseline does not admit.
const UNSPECIFIED_SPS: &str =
    "42010101600000030090000003000003001ea010206165ba4a4c2f016808000003000800000300f040";

fn hex(text: &str) -> Vec<u8> {
    let text: String = text.split_whitespace().collect();
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}
fn config() -> CodecConfiguration {
    CodecConfiguration::new_baseline(
        CodecConfigurationGeneration::INITIAL,
        CodedGeometry::new(&ProtocolLimits::ABSOLUTE, 128, 96, 128, 96, 2).unwrap(),
        ColorInfo::sdr_bt709(),
        GopPolicy::baseline_for_frame_rate(30).unwrap(),
    )
    .unwrap()
}
/// Annex B access units, split at each access unit delimiter (NAL 35).
fn annex_b_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 4 < stream.len() {
        if stream[i..i + 4] == [0, 0, 0, 1] && (stream[i + 4] >> 1) & 0x3f == 35 {
            starts.push(i);
        }
        i += 1;
    }
    starts.push(stream.len());
    starts.windows(2).map(|w| &stream[w[0]..w[1]]).collect()
}
/// What the host sends: SEI stripped, four-byte lengths, typed identities.
fn units() -> Vec<EncodedAccessUnit> {
    let limits = ProtocolLimits::ABSOLUTE;
    let stream = hex(STREAM_HEX);
    annex_b_units(&stream)
        .into_iter()
        .enumerate()
        .map(|(n, au)| {
            let n = u64::try_from(n).unwrap();
            let bytes =
                annex_b_to_length_prefixed(&strip_encoder_sei(au).unwrap(), limits).unwrap();
            let kind = if n == 0 {
                FrameKind::Idr {
                    recovery: RecoveryGeneration::INITIAL,
                }
            } else {
                FrameKind::Predicted {
                    references: FrameId::from_raw(n - 1),
                }
            };
            EncodedAccessUnit::new(
                &limits,
                FrameId::from_raw(n),
                kind,
                CodecConfigurationGeneration::INITIAL,
                n * 33_333,
                bytes,
            )
            .unwrap()
        })
        .collect()
}
/// The record the host derives from its first IDR.
fn record(first: &EncodedAccessUnit) -> Vec<u8> {
    let mut guard = HevcGuard::new(config(), ProtocolLimits::ABSOLUTE, 4).unwrap();
    guard.validate_length_prefixed(first.bytes(), true).unwrap();
    guard.decoder_record().unwrap().bytes().to_vec()
}

#[test]
fn admitted_record_yields_exactly_its_three_parameter_sets() {
    let units = units();
    assert_eq!(units.len(), 3);
    let record = record(&units[0]);
    let [vps, sps, pps] = parameter_sets(&record).unwrap();
    assert_eq!(
        [vps[0] >> 1, sps[0] >> 1, pps[0] >> 1],
        [32, 33, 34],
        "NAL types in order"
    );
    // The same bytes the encoder wrote in band.
    let stream = hex(STREAM_HEX);
    let stream = annex_b_units(&stream)[0];
    for set in [vps, sps, pps] {
        assert!(stream.windows(set.len()).any(|w| w == set));
    }
    let mut longer = record.clone();
    longer.push(0);
    assert_eq!(parameter_sets(&longer), None);
    assert_eq!(parameter_sets(&record[..record.len() - 1]), None);
}

#[test]
fn videotoolbox_decodes_the_admitted_stream_into_the_metal_surface() {
    let units = units();
    let surface = Surface::offscreen(128, 96).unwrap();
    let mut decoder = HevcDecoder::new(
        surface,
        config(),
        ProtocolLimits::ABSOLUTE,
        &record(&units[0]),
    )
    .unwrap();
    for unit in &units {
        decoder.submit(unit).unwrap();
    }
    assert_eq!(decoder.surface().presented(), 3);
    eprintln!("videotoolbox hardware decoder: {:?}", decoder.hardware());
}

/// A canonical-layout record carrying `sps` between the fixture's own VPS
/// and PPS.
fn record_with_sps(sps: &[u8]) -> Vec<u8> {
    let good = record(&units()[0]);
    let [vps, _, pps] = parameter_sets(&good).unwrap();
    let mut out = good[..23].to_vec();
    for (kind, set) in [(32_u8, vps), (33, sps), (34, pps)] {
        out.extend_from_slice(&[0x80 | kind, 0, 1]);
        out.extend_from_slice(&u16::try_from(set.len()).unwrap().to_be_bytes());
        out.extend_from_slice(set);
    }
    out
}

#[test]
fn record_outside_the_admitted_subset_is_refused_before_videotoolbox() {
    let good = record(&units()[0]);
    let [_, sps, _] = parameter_sets(&good).unwrap();
    // Positive control: the same construction with the fixture's own SPS is
    // byte-identical to the admitted record and configures VideoToolbox.
    assert_eq!(record_with_sps(sps), good);
    let surface = Surface::offscreen(128, 96).unwrap();
    assert!(HevcDecoder::new(surface, config(), ProtocolLimits::ABSOLUTE, &good).is_ok());
    // Planted negative: only the SPS changes, to the same encoder's SPS
    // without a colour description. The layout parser accepts it; the
    // guard refuses the colour, and VideoToolbox is never configured.
    let bad = record_with_sps(&hex(UNSPECIFIED_SPS));
    assert!(parameter_sets(&bad).is_some());
    assert_eq!(
        HevcGuard::from_decoder_record(config(), ProtocolLimits::ABSOLUTE, 4, &bad).err(),
        Some(fr_media::hevc::HevcError::ColorMismatch)
    );
    let surface = Surface::offscreen(128, 96).unwrap();
    let refused = HevcDecoder::new(surface, config(), ProtocolLimits::ABSOLUTE, &bad);
    assert_eq!(refused.err(), Some(NativeError::InvalidConfiguration));
}

#[test]
fn replayed_reordered_or_foreign_units_are_refused_without_decoding() {
    let units = units();
    let surface = Surface::offscreen(128, 96).unwrap();
    let mut decoder = HevcDecoder::new(
        surface,
        config(),
        ProtocolLimits::ABSOLUTE,
        &record(&units[0]),
    )
    .unwrap();
    // A predicted picture before any IDR has nothing to reference.
    assert_eq!(
        decoder.submit(&units[1]),
        Err(NativeError::UnsupportedBitstream)
    );
    decoder.submit(&units[0]).unwrap();
    assert_eq!(decoder.submit(&units[0]), Err(NativeError::StaleGeneration));
    // Unit 2 names unit 1 as its reference, which was never decoded.
    assert_eq!(
        decoder.submit(&units[2]),
        Err(NativeError::UnsupportedBitstream)
    );
    let foreign = EncodedAccessUnit::new(
        &ProtocolLimits::ABSOLUTE,
        FrameId::from_raw(1),
        units[1].kind(),
        CodecConfigurationGeneration::INITIAL.next().unwrap(),
        0,
        units[1].bytes().to_vec(),
    )
    .unwrap();
    assert_eq!(decoder.submit(&foreign), Err(NativeError::StaleGeneration));
    assert_eq!(
        decoder.surface().presented(),
        1,
        "only the IDR was presented"
    );
    decoder.submit(&units[1]).unwrap();
    decoder.submit(&units[2]).unwrap();
    assert_eq!(decoder.surface().presented(), 3);
}

#[test]
fn modifier_changes_become_one_press_and_one_release() {
    let mut m = Modifiers::default();
    let mut events = Vec::new();
    // Left shift down (device bit 0x02 + shift flag), duplicate, then up.
    m.change(0x38, (1 << 17) | 0x02, &mut events);
    m.change(0x38, (1 << 17) | 0x02, &mut events);
    // Right shift down while left is held, then left up while right is held.
    m.change(0x3c, (1 << 17) | 0x06, &mut events);
    m.change(0x38, (1 << 17) | 0x04, &mut events);
    m.change(0x3c, 0, &mut events);
    let keys: Vec<_> = events
        .iter()
        .map(|e| match *e {
            WindowEvent::Key { usage, pressed, .. } => (usage, pressed),
            ref other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        keys,
        [(0xe1, true), (0xe5, true), (0xe1, false), (0xe5, false)]
    );
    // A keyboard with no device bits falls back to the shared flag.
    events.clear();
    m.change(0x3b, 1 << 18, &mut events);
    m.change(0x3b, 0, &mut events);
    assert_eq!(events.len(), 2);
    // Caps Lock reports a toggle; each change is one tap.
    events.clear();
    m.change(0x39, 1 << 16, &mut events);
    assert_eq!(events.len(), 2);
    events.clear();
    m.change(0x3f, 1 << 23, &mut events);
    assert_eq!(events, [WindowEvent::UnmappedKey { keycode: 0x3f }]);
}

#[test]
fn picture_points_map_to_clamped_remote_pixels() {
    let p = |x, y| PicturePoint { x, y, inside: true };
    assert_eq!(p(0.0, 0.0).pixel(1920, 1080), (0, 0));
    assert_eq!(p(0.5, 0.5).pixel(1920, 1080), (960, 540));
    assert_eq!(p(0.999_999, 0.999_999).pixel(1920, 1080), (1919, 1079));
    assert_eq!(p(1.5, -0.2).pixel(1920, 1080), (1919, 0));
    assert_eq!(p(f64::NAN, f64::INFINITY).pixel(1920, 1080), (0, 0));
}
