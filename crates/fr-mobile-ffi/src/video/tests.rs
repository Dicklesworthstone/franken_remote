//! Real platform decode of a real libx265 stream (fr-native's macOS viewer
//! fixture): `VideoToolbox` on the macOS host (the same C glue the iOS app
//! links), Media Foundation on Windows.
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
// The fixture's SPS after FFmpeg's hevc_metadata=video_full_range_flag=1.
// Only the range flag changes; the VPS, PPS and encoded picture are retained.
const FULL_RANGE_SPS: &str =
    "42010101600000030090000003000003001ea010206165ba4a4c2f016e02020208000003000800000300f040";

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
/// Releases a +1 reference the slot handed out.
pub(crate) fn release(raw: *mut c_void) {
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    // SAFETY: the slot returned a +1 reference for this caller to release.
    unsafe {
        super::backend::fr_vt_image_release(raw);
    }
    #[cfg(windows)]
    // SAFETY: the slot returned a +1 `IMFSample` reference; `from_raw`
    // takes it over and releases it on drop.
    drop(unsafe {
        <windows::Win32::Media::MediaFoundation::IMFSample as windows::core::Interface>::from_raw(
            raw,
        )
    });
}
fn decoder() -> Decoder {
    Decoder::new(config(), ProtocolLimits::ABSOLUTE, &record(&units()[0])).unwrap()
}

/// One real native picture with its own admitted colour range. This is used
/// by the shared-session regression too; no fake native image enters the slot.
pub(crate) fn picture(full_range: bool) -> Image {
    let first = units().remove(0);
    let mut config = config();
    let mut record = record(&first);
    let mut bytes = first.bytes().to_vec();
    if full_range {
        let [_, original_sps, _] = parameter_sets(&record).unwrap();
        let full_sps = hex(FULL_RANGE_SPS);
        assert_eq!(original_sps.len(), full_sps.len());
        let offset = bytes
            .windows(original_sps.len())
            .position(|candidate| candidate == original_sps)
            .unwrap();
        bytes[offset..offset + full_sps.len()].copy_from_slice(&full_sps);
        record = record_with_sps(&full_sps);
        config = CodecConfiguration::new_baseline(
            config.generation(),
            config.geometry(),
            ColorInfo {
                range: ColorRange::Full,
                ..config.color()
            },
            config.gop(),
        )
        .unwrap();
    }
    let unit = EncodedAccessUnit::new(
        &ProtocolLimits::ABSOLUTE,
        first.frame(),
        first.kind(),
        first.config_generation(),
        first.capture_micros(),
        bytes,
    )
    .unwrap();
    let mut decoder = Decoder::new(config, ProtocolLimits::ABSOLUTE, &record).unwrap();
    assert_eq!(decoder.full_range(), full_range);
    let image = decoder.submit(&unit, true).unwrap().unwrap();
    assert_eq!(image.describe().2 == NV12_FULL_RANGE, full_range);
    image
}

#[test]
fn a_retained_picture_carries_its_own_limited_or_full_range() {
    let slot = FrameSlot::default();
    slot.begin_attempt().unwrap();
    for (frame, range) in [(1, false), (2, true), (3, false)] {
        slot.publish_for(slot.app_id(0), picture(range), frame)
            .unwrap();
        let (raw, info) = slot.take_newer(0).unwrap();
        release(raw);
        assert_eq!(info.full_range, range);
        assert_eq!(info.frame, slot.app_id(frame));
    }
}

#[test]
fn videotoolbox_decodes_into_io_surface_nv12_pictures_of_the_stream_size() {
    let units = units();
    assert_eq!(units.len(), 3);
    let mut decoder = decoder();
    assert_eq!(decoder.picture(), (128, 96));
    assert!(!decoder.full_range(), "the fixture is BT.709 limited range");
    let slot = FrameSlot::default();
    slot.begin_attempt().unwrap();
    for unit in &units {
        let image = decoder
            .submit(unit, true)
            .unwrap()
            .expect("a presented picture");
        let (width, height, format, io_surface) = image.describe();
        assert_eq!((width, height), (128, 96));
        assert_eq!(format, NV12_VIDEO_RANGE);
        #[cfg(any(target_os = "ios", target_os = "macos"))]
        assert!(io_surface, "Metal needs an IOSurface-backed buffer");
        // DXVA pictures are D3D11 textures; a software transform's are not.
        #[cfg(windows)]
        assert_eq!(Some(io_surface), decoder.hardware(), "texture vs DXVA");
        // testsrc2 is colour bars and gradients: never a flat black or
        // white picture. Limited-range luma stays inside 16..=235.
        let mean = image.mean_luma().unwrap();
        assert!((40.0..200.0).contains(&mean), "{mean}");
        slot.publish_for(slot.app_id(0), image, unit.frame().as_raw())
            .unwrap();
    }
    assert_eq!(slot.presented(), 3);
    eprintln!("platform hardware decoder: {:?}", decoder.hardware());
}

#[test]
fn a_decode_only_unit_returns_no_picture_but_advances_the_chain() {
    let units = units();
    let mut decoder = decoder();
    assert!(decoder.submit(&units[0], false).unwrap().is_none());
    // Unit 1 references unit 0, which decoded without presenting.
    assert!(decoder.submit(&units[1], true).unwrap().is_some());
}

#[cfg(windows)]
#[test]
fn windows_output_owns_its_frame_even_when_capture_timestamps_repeat_or_overflow() {
    use windows::Win32::Media::MediaFoundation::IMFSample;
    use windows::core::Interface;

    let mut decoder = decoder();
    for original in units() {
        let unit = EncodedAccessUnit::new(
            &ProtocolLimits::ABSOLUTE,
            original.frame(),
            original.kind(),
            original.config_generation(),
            u64::MAX,
            original.bytes().to_vec(),
        )
        .unwrap();
        let image = decoder.submit(&unit, true).unwrap().unwrap();
        // SAFETY: retained_raw returns a +1 sample reference for this caller;
        // the wrapper owns and releases it after the read-only timestamp query.
        let sample = unsafe { IMFSample::from_raw(image.retained_raw()) };
        // SAFETY: the sample is live and owned until this scope ends.
        let time = unsafe { sample.GetSampleTime() }.unwrap();
        assert_eq!(time, i64::try_from(unit.frame().as_raw()).unwrap());
        assert_eq!(unit.capture_micros(), u64::MAX);
    }
}

#[test]
fn the_slot_hands_out_only_newer_pictures_and_tracks_drawn_frames() {
    let units = units();
    let mut decoder = decoder();
    let slot = FrameSlot::default();
    slot.begin_attempt().unwrap();
    assert!(slot.take_newer(0).is_none());
    let image = decoder.submit(&units[0], true).unwrap().unwrap();
    slot.publish_for(slot.app_id(0), image, 7).unwrap();
    let (raw, info) = slot.take_newer(0).unwrap();
    assert!(!raw.is_null());
    release(raw);
    assert_eq!(
        info,
        FrameInfo {
            sequence: 1,
            frame: slot.app_id(7),
            width: 128,
            height: 96,
            full_range: false
        }
    );
    assert!(slot.take_newer(info.sequence).is_none());
    slot.mark_drawn(slot.app_id(7));
    slot.mark_drawn(slot.app_id(3));
    assert_eq!(
        slot.drawn(),
        slot.app_id(7),
        "a late report never lowers the drawn frame"
    );
    slot.clear();
    assert!(slot.take_newer(0).is_none());
}

#[test]
fn a_delayed_native_picture_cannot_be_relabelled_into_a_new_attempt() {
    let units = units();
    let mut decoder = decoder();
    let slot = FrameSlot::default();
    slot.begin_attempt().unwrap();
    let original = slot.app_id(0);
    let delayed = decoder.submit(&units[0], true).unwrap().unwrap();
    slot.begin_attempt().unwrap();
    let current = slot.app_id(0);
    assert_ne!(original, current);
    let fresh = decoder.submit(&units[1], true).unwrap().unwrap();
    slot.publish_for(current, fresh, 3).unwrap();
    assert_eq!(slot.sequence(), 1);
    assert!(matches!(
        slot.publish_for(original, delayed, 500),
        Err(Error::StaleGeneration)
    ));
    assert_eq!(
        slot.sequence(),
        1,
        "late native output is not publication evidence"
    );
    assert_eq!(slot.presented(), 1);
    let (raw, info) = slot.take_newer(0).unwrap();
    release(raw);
    assert_eq!(info.frame, current | 3);
    slot.mark_drawn(original | 0x1f4);
    assert_eq!(slot.drawn(), 0);
    slot.mark_drawn(info.frame);
    assert_eq!(slot.drawn(), info.frame);
    slot.clear();
    let draining = decoder.submit(&units[2], true).unwrap().unwrap();
    assert!(matches!(
        slot.publish_for(current, draining, 4),
        Err(Error::Closed)
    ));
    assert!(matches!(slot.begin_attempt(), Err(Error::Closed)));
    assert!(slot.take_newer(0).is_none());
    assert_eq!(slot.presented(), 1);
}

#[test]
fn attempt_exhaustion_refuses_instead_of_reusing_an_old_identity() {
    let slot = FrameSlot::default();
    slot.generation
        .store(u64::MAX >> RAW_BITS, Ordering::Release);
    let before = slot.app_id(0);
    assert!(matches!(slot.begin_attempt(), Err(Error::Closed)));
    assert_eq!(slot.app_id(0), before);
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

#[test]
fn a_cursor_reset_fences_old_attempts_and_requires_a_new_shape() {
    let mut cursor = Cursor::default();
    cursor.reset(11, 5).unwrap();
    let update = CursorUpdate::Shape {
        x: 4,
        y: 5,
        width: 2,
        height: 2,
        hotspot_x: 1,
        hotspot_y: 1,
        rgba: &[255; 16],
    };
    cursor.apply_for(11, &update).unwrap();
    let original_shape = cursor.shape_generation;
    cursor
        .apply_for(11, &CursorUpdate::Move { x: 20, y: 21 })
        .unwrap();
    assert_eq!(cursor.shape_generation, original_shape);
    let before_reset = cursor.generation;
    cursor.reset(12, 8).unwrap();
    assert!(cursor.generation > before_reset);
    assert!(cursor.shape_generation > original_shape);
    assert_eq!((cursor.attempt, cursor.frame_after), (12, 8));
    assert!(!cursor.visible);
    assert_eq!(cursor.rgba, [] as [u8; 0]);
    assert_eq!(cursor.apply_for(11, &update), Err(Error::StaleGeneration));
    assert_eq!(
        cursor.apply_for(12, &CursorUpdate::Move { x: 4, y: 5 }),
        Err(Error::InvalidConfiguration)
    );
    cursor.apply_for(12, &update).unwrap();
    // A decoder change within the SAME connection also retires its shape.
    cursor.reset(12, 10).unwrap();
    assert!(!cursor.visible);
    assert_eq!(cursor.rgba, [] as [u8; 0]);
    assert_eq!(cursor.frame_after, 10);
    cursor.reset(0, 10).unwrap();
    assert_eq!(cursor.apply_for(0, &update), Err(Error::StaleGeneration));
}

#[test]
fn exhausted_cursor_revisions_refuse_without_partially_changing_state() {
    let mut cursor = Cursor {
        generation: u64::MAX,
        visible: true,
        ..Cursor::default()
    };
    assert_eq!(cursor.apply(&CursorUpdate::Hidden), Err(Error::Closed));
    assert!(cursor.visible);
    assert_eq!(cursor.reset(1, 0), Err(Error::Closed));
    assert_eq!(cursor.attempt, 0);
}
