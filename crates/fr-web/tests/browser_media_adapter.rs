//! Executes production media admission/custody; no native browser is simulated
//! as evidence of HEVC support or an authenticated browser desktop.
use fr_core::{ids::CodecConfigurationGeneration, limits::ProtocolLimits};
use fr_media::{
    config::{CodecConfiguration, CodedGeometry, ColorInfo, GopPolicy},
    hevc::HevcGuard,
};
use fr_web::media_adapter::{MediaAdapter, MediaAdapterError};

const VPS: &str = "40010c01ffff01600000030090000003000003003cba0240";
const SPS: &str =
    "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04";
const PPS: &str = "4401c0718112";
const IDR: &str = "2801ade06702f86753c11ead2f1f6a69";

fn picture() -> Vec<u8> {
    let mut bytes = Vec::new();
    for nal in [VPS, SPS, PPS, IDR] {
        bytes.extend_from_slice(&u32::try_from(nal.len() / 2).unwrap().to_be_bytes());
        bytes.extend(
            nal.as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u8::from_str_radix(core::str::from_utf8(pair).unwrap(), 16).unwrap()),
        );
    }
    bytes
}

fn configuration(generation: u64) -> (String, Vec<u8>) {
    let config = CodecConfiguration::new_baseline(
        CodecConfigurationGeneration::from_raw(generation),
        CodedGeometry::new(&ProtocolLimits::ABSOLUTE, 320, 240, 320, 240, 2).unwrap(),
        ColorInfo::sdr_bt709(),
        GopPolicy::baseline_for_frame_rate(30).unwrap(),
    )
    .unwrap();
    let mut guard = HevcGuard::new(config, ProtocolLimits::ABSOLUTE, 4).unwrap();
    guard.validate_length_prefixed(&picture(), true).unwrap();
    let record = guard.decoder_record().unwrap();
    let metadata = serde_json::json!({
        "generation": generation.to_string(), "codedWidth": 320, "codedHeight": 240,
        "cropWidth": 320, "cropHeight": 240, "fps": 30,
        "primaries": 1, "transfer": 1, "matrix": 1, "fullRange": false,
        "decodedPictures": 4, "codec": record.codec().replacen("hvc1.","hev1.",1),
    })
    .to_string();
    (metadata, record.bytes().to_vec())
}

fn configured(adapter: &mut MediaAdapter, generation: u64) -> u64 {
    let (metadata, hvcc) = configuration(generation);
    let prepared = adapter.prepare_configuration(&metadata, &hvcc).unwrap();
    let serial = prepared.decoder.parse().unwrap();
    adapter.configured(serial).unwrap();
    serial
}

#[test]
fn escaped_retired_output_stays_charged_after_original_decoder_closure() {
    let mut adapter = MediaAdapter::new(4, 4096);
    let old = configured(&mut adapter, 0);
    let first = adapter
        .prepare_chunk(old, 9, 123, true, &picture())
        .unwrap();
    adapter.retire();
    let current = configured(&mut adapter, 0);
    assert_ne!(old, current);
    let current_frame = adapter
        .prepare_chunk(current, 9, 456, true, &picture())
        .unwrap();
    let stale = adapter.output(old, first.callback).unwrap();
    assert!(!stale.can_present && !stale.first_decoded);
    assert_eq!(stale.source_timestamp_us, "123");
    assert_eq!(adapter.decoder_closed(old).unwrap().len(), 0);
    assert_eq!(adapter.snapshot().inflight_frames, 2);
    assert!(adapter.presented(old, first.callback, 1000, true).is_err());
    assert!(
        adapter
            .output(current, current_frame.callback)
            .unwrap()
            .can_present
    );
    let released = adapter.frame_closed(old, first.callback).unwrap();
    assert_eq!(released.decoder, old.to_string());
    assert_eq!(released.remaining_frames, 1);
    assert!(adapter.frame_closed(old, first.callback).is_err());
    adapter
        .presented(current, current_frame.callback, 1000, true)
        .unwrap();
}

#[test]
fn pending_inputs_and_outputs_share_exact_count_and_byte_bounds() {
    let bytes = picture();
    let mut adapter = MediaAdapter::new(4, bytes.len() as u64 * 2);
    let decoder = configured(&mut adapter, 0);
    let first = adapter.prepare_chunk(decoder, 1, 0, true, &bytes).unwrap();
    assert_eq!(
        adapter.frame_closed(decoder, first.callback).err(),
        Some(MediaAdapterError::State)
    );
    adapter.output(decoder, first.callback).unwrap();
    adapter.prepare_chunk(decoder, 2, 0, true, &bytes).unwrap();
    assert_eq!(
        adapter
            .prepare_chunk(decoder, 3, 0, true, &bytes)
            .err()
            .unwrap()
            .code(),
        "browser_media_capacity"
    );
    let releases = adapter.decoder_closed(decoder).unwrap();
    assert_eq!(releases.len(), 1);
    assert_eq!(releases[0].frame_id, "2");
    assert_eq!(adapter.snapshot().inflight_bytes, bytes.len() as u64);
    assert!(adapter.reset_session().is_err());
    adapter.frame_closed(decoder, first.callback).unwrap();
    adapter.reset_session().unwrap();
    let next = configured(&mut adapter, 0);
    let fresh = adapter.prepare_chunk(next, 1, 0, true, &bytes).unwrap();
    assert!(fresh.callback > first.callback);
    assert!(adapter.output(decoder, fresh.callback).is_err());
}

#[test]
fn configuration_and_codec_mismatch_refuse_before_replacing_the_owner() {
    let mut adapter = MediaAdapter::new(4, 4096);
    let original = configured(&mut adapter, 0);
    let (metadata, hvcc) = configuration(1);
    let mut bad: serde_json::Value = serde_json::from_str(&metadata).unwrap();
    for (field, value) in [
        ("codec", serde_json::json!("hev1.1.6.L999.B0")),
        ("generation", serde_json::json!("18446744073709551616")),
        ("codedWidth", serde_json::json!(8193)),
        ("decodedPictures", serde_json::json!(12)),
    ] {
        let old = bad[field].clone();
        bad[field] = value;
        assert!(
            adapter
                .prepare_configuration(&bad.to_string(), &hvcc)
                .is_err()
        );
        assert_eq!(
            adapter.snapshot().active_decoder,
            Some(original.to_string())
        );
        bad[field] = old;
    }
    assert!(matches!(
        adapter.prepare_configuration(&" ".repeat(1025), &hvcc),
        Err(MediaAdapterError::Bounds)
    ));
    assert!(adapter.prepare_configuration(&metadata, &[0; 23]).is_err());
}

#[test]
fn duplicate_output_and_false_visible_presentation_do_not_release_custody() {
    let mut adapter = MediaAdapter::new(1, 4096);
    let decoder = configured(&mut adapter, 0);
    let chunk = adapter
        .prepare_chunk(decoder, u64::MAX, u64::MAX, true, &picture())
        .unwrap();
    let output = adapter.output(decoder, chunk.callback).unwrap();
    assert_eq!(output.frame_id, u64::MAX.to_string());
    assert_eq!(output.source_timestamp_us, u64::MAX.to_string());
    assert!(output.first_decoded);
    assert!(adapter.output(decoder, chunk.callback).is_err());
    assert!(
        adapter
            .presented(decoder, chunk.callback, 1, false)
            .is_err()
    );
    assert_eq!(adapter.snapshot().inflight_frames, 1);
    adapter.presented(decoder, chunk.callback, 1, true).unwrap();
    assert!(adapter.presented(decoder, chunk.callback, 2, true).is_err());
    assert_eq!(adapter.snapshot().inflight_frames, 1);
}

#[test]
fn unowned_native_custody_quarantines_admission_and_retains_all_reservations() {
    let mut adapter = MediaAdapter::new(4, 4096);
    let decoder = configured(&mut adapter, 0);
    let chunk = adapter
        .prepare_chunk(decoder, 1, 7, true, &picture())
        .unwrap();
    adapter.output(decoder, chunk.callback).unwrap();
    adapter
        .prepare_chunk(decoder, 2, 8, true, &picture())
        .unwrap();
    adapter.quarantine();
    adapter.quarantine();
    let (metadata, hvcc) = configuration(1);
    assert_eq!(
        adapter.prepare_configuration(&metadata, &hvcc).err(),
        Some(MediaAdapterError::Quarantined)
    );
    assert_eq!(
        adapter.prepare_chunk(decoder, 3, 9, true, &picture()).err(),
        Some(MediaAdapterError::Quarantined)
    );
    assert_eq!(
        adapter.frame_closed(decoder, chunk.callback).err(),
        Some(MediaAdapterError::Quarantined)
    );
    assert_eq!(
        adapter.decoder_closed(decoder).err(),
        Some(MediaAdapterError::Quarantined)
    );
    assert_eq!(adapter.reset_session(), Err(MediaAdapterError::Quarantined));
    assert!(adapter.snapshot().quarantined);
    assert_eq!(adapter.snapshot().active_decoder, None);
    assert_eq!(adapter.snapshot().retained_decoders, 1);
    assert_eq!(adapter.snapshot().inflight_frames, 2);
    assert_eq!(
        adapter.snapshot().inflight_bytes,
        picture().len() as u64 * 2
    );
}
