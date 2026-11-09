//! Real `VideoToolbox` decode of a real libx265 stream (fr-native's macOS
//! viewer fixture), on the macOS host: the same C glue the iOS app links.
use super::*;
use fr_core::ids::{CodecConfigurationGeneration, RecoveryGeneration};
use fr_media::config::{CodedGeometry, ColorInfo, GopPolicy};
use fr_media::hevc::framing::annex_b_to_length_prefixed;
use fr_media::hevc::strip_encoder_sei;

// Real FFmpeg 8.1.2 / libx265 4.2 Main8 4:2:0 BT.709 limited range, 128x96
// testsrc2, one IDR and two P pictures (see fr-native's macos/tests.rs).
const STREAM_HEX: &str = include_str!("../../../fr-native/src/macos/tiny128.hex");
// The same encoder's SPS without a colour description (refused by the guard).
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
pub(crate) fn units() -> Vec<EncodedAccessUnit> {
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
fn record(first: &EncodedAccessUnit) -> Vec<u8> {
    let mut guard = HevcGuard::new(config(), ProtocolLimits::ABSOLUTE, 4).unwrap();
    guard.validate_length_prefixed(first.bytes(), true).unwrap();
    guard.decoder_record().unwrap().bytes().to_vec()
}
fn decoder() -> Decoder {
    Decoder::new(config(), ProtocolLimits::ABSOLUTE, &record(&units()[0])).unwrap()
}

#[test]
fn videotoolbox_decodes_into_io_surface_nv12_pictures_of_the_stream_size() {
    let units = units();
    assert_eq!(units.len(), 3);
    let mut decoder = decoder();
    assert_eq!(decoder.picture(), (128, 96));
    assert!(!decoder.full_range(), "the fixture is BT.709 limited range");
    let slot = FrameSlot::default();
    for unit in &units {
        let image = decoder
            .submit(unit, true)
            .unwrap()
            .expect("a presented picture");
        let (width, height, format, io_surface) = image.describe();
        assert_eq!((width, height), (128, 96));
        assert_eq!(format, NV12_VIDEO_RANGE);
        assert!(io_surface, "Metal needs an IOSurface-backed buffer");
        // testsrc2 is colour bars and gradients: never a flat black or
        // white picture. Limited-range luma stays inside 16..=235.
        let mean = image.mean_luma().unwrap();
        assert!((40.0..200.0).contains(&mean), "{mean}");
        slot.publish(image, unit.frame().as_raw());
    }
    assert_eq!(slot.presented(), 3);
    eprintln!("videotoolbox hardware decoder: {:?}", decoder.hardware());
}

#[test]
fn a_decode_only_unit_returns_no_picture_but_advances_the_chain() {
    let units = units();
    let mut decoder = decoder();
    assert!(decoder.submit(&units[0], false).unwrap().is_none());
    // Unit 1 references unit 0, which decoded without presenting.
    assert!(decoder.submit(&units[1], true).unwrap().is_some());
}

#[test]
fn the_slot_hands_out_only_newer_pictures_and_tracks_drawn_frames() {
    let units = units();
    let mut decoder = decoder();
    let slot = FrameSlot::default();
    assert!(slot.take_newer(0, false).is_none());
    let image = decoder.submit(&units[0], true).unwrap().unwrap();
    slot.publish(image, 7);
    let (raw, info) = slot.take_newer(0, false).unwrap();
    assert!(!raw.is_null());
    // SAFETY: the slot returned a +1 reference for this caller to release.
    unsafe { fr_vt_image_release(raw) };
    assert_eq!(
        info,
        FrameInfo {
            sequence: 1,
            frame: 7,
            width: 128,
            height: 96,
            full_range: false
        }
    );
    assert!(slot.take_newer(info.sequence, false).is_none());
    slot.mark_drawn(7);
    slot.mark_drawn(3);
    assert_eq!(
        slot.drawn(),
        7,
        "a late report never lowers the drawn frame"
    );
    slot.clear();
    assert!(slot.take_newer(0, false).is_none());
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
fn a_record_outside_the_admitted_subset_never_reaches_videotoolbox() {
    let good = record(&units()[0]);
    let [_, sps, _] = parameter_sets(&good).unwrap();
    // Positive control: rebuilt with its own SPS it is the admitted record.
    assert_eq!(record_with_sps(sps), good);
    assert!(Decoder::new(config(), ProtocolLimits::ABSOLUTE, &good).is_ok());
    // Planted negative: only the SPS changes (no colour description).
    let bad = record_with_sps(&hex(UNSPECIFIED_SPS));
    assert!(parameter_sets(&bad).is_some());
    assert_eq!(
        Decoder::new(config(), ProtocolLimits::ABSOLUTE, &bad).err(),
        Some(Error::InvalidConfiguration)
    );
}

#[test]
fn replayed_reordered_or_foreign_units_are_refused_without_decoding() {
    let units = units();
    let mut decoder = decoder();
    assert_eq!(
        decoder.submit(&units[1], true).err(),
        Some(Error::UnsupportedBitstream)
    );
    decoder.submit(&units[0], true).unwrap();
    assert_eq!(
        decoder.submit(&units[0], true).err(),
        Some(Error::StaleGeneration)
    );
    assert_eq!(
        decoder.submit(&units[2], true).err(),
        Some(Error::UnsupportedBitstream)
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
    assert_eq!(
        decoder.submit(&foreign, true).err(),
        Some(Error::StaleGeneration)
    );
    decoder.submit(&units[1], true).unwrap();
    decoder.submit(&units[2], true).unwrap();
}

#[test]
fn cursor_updates_need_a_checked_shape_first() {
    let mut cursor = Cursor::default();
    assert_eq!(
        cursor.apply(&CursorUpdate::Move { x: 1, y: 1 }),
        Err(Error::InvalidConfiguration)
    );
    let rgba = vec![255_u8; 4 * 4 * 4];
    let bad = CursorUpdate::Shape {
        x: 0,
        y: 0,
        width: 4,
        height: 4,
        hotspot_x: 4,
        hotspot_y: 0,
        rgba: &rgba,
    };
    assert_eq!(cursor.apply(&bad), Err(Error::InvalidConfiguration));
    assert_eq!(cursor.generation, 0);
    cursor
        .apply(&CursorUpdate::Shape {
            x: 10,
            y: 12,
            width: 4,
            height: 4,
            hotspot_x: 1,
            hotspot_y: 1,
            rgba: &rgba,
        })
        .unwrap();
    cursor.apply(&CursorUpdate::Move { x: 20, y: 21 }).unwrap();
    cursor.apply(&CursorUpdate::Hidden).unwrap();
    assert_eq!((cursor.x, cursor.y, cursor.visible), (20, 21, false));
    assert_eq!(cursor.generation, 3);
    assert_eq!(cursor.rgba.len(), 64);
}
