//! Native-resource custody and callback identity, independently of session authority.
use fr_core::{
    ids::{CodecConfigurationGeneration, HostBootId, OsSessionId, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_media::{
    config::{CodecConfiguration, CodedGeometry, ColorInfo, GopPolicy},
    hevc::{HevcError, HevcGuard},
};
use fr_web::{
    BrowserCreditError, BrowserCreditTracker, BrowserDecoderStage, BrowserHevcError,
    hevc::{BrowserHevcPipeline, DecoderInstance, PreparedWebCodecsChunk},
};
use fr_wire::{
    WireError,
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    receiver_pressure::{self, ChannelRole},
};

// Real x265 baseline fixture, also exercised by fr-media's parameter-set guard.
const VPS: &str = "40010c01ffff01600000030090000003000003003cba0240";
const SPS: &str =
    "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04";
const PPS: &str = "4401c0718112";
const IDR: &str = "2801ade06702f86753c11ead2f1f6a69";
const P1: &str = "0201d009788230da9e2ce74eaffe0295";

fn access_unit(nals: &[&str]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for nal in nals {
        bytes.extend_from_slice(&u32::try_from(nal.len() / 2).unwrap().to_be_bytes());
        for pair in nal.as_bytes().as_chunks::<2>().0 {
            bytes.push(u8::from_str_radix(core::str::from_utf8(pair).unwrap(), 16).unwrap());
        }
    }
    bytes
}

fn config(generation: u64) -> CodecConfiguration {
    CodecConfiguration::new_baseline(
        CodecConfigurationGeneration::from_raw(generation),
        CodedGeometry::new(&ProtocolLimits::ABSOLUTE, 320, 240, 320, 240, 2).unwrap(),
        ColorInfo::sdr_bt709(),
        GopPolicy::baseline_for_frame_rate(30).unwrap(),
    )
    .unwrap()
}

fn hvcc() -> Vec<u8> {
    let mut guard = HevcGuard::new(config(0), ProtocolLimits::ABSOLUTE, 4).unwrap();
    guard
        .validate_length_prefixed(&access_unit(&[VPS, SPS, PPS, IDR]), true)
        .unwrap();
    guard.decoder_record().unwrap().bytes().to_vec()
}

fn configure(pipeline: &mut BrowserHevcPipeline, generation: u64) -> DecoderInstance {
    let prepared = pipeline
        .admit_decoder_configuration(config(generation), ProtocolLimits::ABSOLUTE, 4, &hvcc())
        .unwrap();
    assert_eq!(prepared.generation, prepared.decoder.generation);
    pipeline.on_decoder_configured(prepared.decoder).unwrap();
    prepared.decoder
}

fn admit(
    pipeline: &mut BrowserHevcPipeline,
    decoder: DecoderInstance,
    frame: u64,
) -> PreparedWebCodecsChunk {
    pipeline
        .admit_access_unit(
            decoder,
            frame,
            1_000 + frame,
            true,
            &access_unit(&[VPS, SPS, PPS, IDR]),
        )
        .unwrap()
}

#[test]
fn decode_and_visible_presentation_are_distinct_one_time_events() {
    let mut pipeline = BrowserHevcPipeline::new(4, 4096);
    let decoder = configure(&mut pipeline, 0);
    let first = admit(&mut pipeline, decoder, 0);
    assert_eq!(first.decoder, decoder);
    assert_eq!(
        pipeline.on_frame_presented(decoder, 0, 2_000, true),
        Err(BrowserHevcError::FrameNotDecoded)
    );
    assert_eq!(
        pipeline.stage(),
        BrowserDecoderStage::Configured { generation: 0 }
    );
    pipeline.on_frame_decoded(decoder, 0).unwrap();
    assert_eq!(
        pipeline.on_frame_decoded(decoder, 0),
        Err(BrowserHevcError::AlreadyDecoded)
    );
    assert_eq!(
        pipeline.on_frame_presented(decoder, 0, 2_000, false),
        Err(BrowserHevcError::HiddenPresentationForbidden)
    );
    pipeline
        .on_frame_presented(decoder, 0, 2_000, true)
        .unwrap();
    assert_eq!(
        pipeline.on_frame_presented(decoder, 0, 3_000, true),
        Err(BrowserHevcError::InvalidPresentationOrder)
    );
    assert!(matches!(
        pipeline.stage(),
        BrowserDecoderStage::Presented {
            last_source_us: 1_000,
            last_presented_us: 2_000,
            ..
        }
    ));
    assert_eq!(pipeline.inflight_frames(), 1);
    assert_eq!(pipeline.inflight_bytes(), first.charged_bytes);
}

#[test]
fn frame_ids_never_reuse_within_one_decoder_even_after_release() {
    let mut pipeline = BrowserHevcPipeline::new(4, 4096);
    let decoder = configure(&mut pipeline, 0);
    admit(&mut pipeline, decoder, 0);
    let before = pipeline.inflight_bytes();
    assert_eq!(
        pipeline.admit_access_unit(decoder, 0, 10_000, false, &access_unit(&[P1])),
        Err(BrowserHevcError::InvalidFrameOrder)
    );
    assert_eq!(pipeline.inflight_bytes(), before);
    pipeline.on_video_frame_closed(decoder, 0).unwrap();
    assert_eq!(
        pipeline.admit_access_unit(
            decoder,
            0,
            10_000,
            true,
            &access_unit(&[VPS, SPS, PPS, IDR])
        ),
        Err(BrowserHevcError::InvalidFrameOrder)
    );
    // A rejected duplicate did not advance or damage the admitted reference chain.
    pipeline
        .admit_access_unit(decoder, 1, 10_000, false, &access_unit(&[P1]))
        .unwrap();
}

#[test]
fn reconfiguration_keeps_old_surfaces_charged_and_scopes_late_closure() {
    let mut pipeline = BrowserHevcPipeline::new(2, 4096);
    let old = configure(&mut pipeline, 0);
    let first = admit(&mut pipeline, old, 0);
    pipeline.on_frame_decoded(old, 0).unwrap();

    let new = configure(&mut pipeline, 1);
    assert!(new.serial > old.serial);
    assert_eq!(pipeline.inflight_bytes(), first.charged_bytes);
    let second = admit(&mut pipeline, new, 0);
    assert_eq!(pipeline.inflight_frames(), 2);
    assert_eq!(
        pipeline.admit_access_unit(new, 1, 2_000, true, &access_unit(&[VPS, SPS, PPS, IDR])),
        Err(BrowserHevcError::DecodeQueueFull)
    );
    assert_eq!(
        pipeline.on_frame_presented(old, 0, 3_000, true),
        Err(BrowserHevcError::StaleGeneration)
    );
    assert_eq!(pipeline.on_decoder_closed(old).unwrap(), []);
    let released = pipeline.on_video_frame_closed(old, 0).unwrap();
    assert_eq!(released.decoder, old);
    assert_eq!(released.freed_bytes, first.charged_bytes);
    assert_eq!(released.remaining_inflight_bytes, second.charged_bytes);
    assert_eq!(
        pipeline.on_video_frame_closed(old, 0),
        Err(BrowserHevcError::UnknownFrameId)
    );
    assert_eq!(pipeline.inflight_frames(), 1);
    pipeline.on_frame_decoded(new, 0).unwrap();
    pipeline.on_frame_presented(new, 0, 3_000, true).unwrap();
}

#[test]
fn decoder_recovery_changes_callback_owner_without_inventing_codec_generation() {
    let mut pipeline = BrowserHevcPipeline::new(4, 4096);
    let old = configure(&mut pipeline, 0);
    admit(&mut pipeline, old, 0);
    pipeline.on_frame_decoded(old, 0).unwrap();
    pipeline.on_decoder_reclaimed(old).unwrap();
    assert_eq!(
        pipeline.on_frame_presented(old, 0, 4_000, true),
        Err(BrowserHevcError::NotConfigured)
    );
    assert_eq!(
        pipeline.on_decoder_configured(old),
        Err(BrowserHevcError::InvalidDecoderState)
    );

    let mut changed = hvcc();
    *changed.last_mut().unwrap() ^= 1;
    assert_eq!(
        pipeline.admit_decoder_configuration(config(0), ProtocolLimits::ABSOLUTE, 4, &changed),
        Err(BrowserHevcError::Hevc(HevcError::ChangedParameterSet))
    );
    assert_eq!(pipeline.active_decoder(), Some(old));
    assert_eq!(pipeline.inflight_frames(), 1);

    let replacement = configure(&mut pipeline, 0);
    assert_eq!(replacement.generation, old.generation);
    assert_eq!(replacement.serial, old.serial + 1);
    assert_eq!(
        pipeline.on_decoder_reclaimed(old),
        Err(BrowserHevcError::StaleGeneration)
    );
    assert_eq!(
        pipeline.on_frame_decoded(old, 0),
        Err(BrowserHevcError::StaleGeneration)
    );
    assert_eq!(
        pipeline.admit_access_unit(replacement, 0, 5_000, false, &access_unit(&[P1])),
        Err(BrowserHevcError::AwaitingIdr)
    );
    admit(&mut pipeline, replacement, 0);
    pipeline.on_video_frame_closed(old, 0).unwrap();
    assert_eq!(pipeline.inflight_frames(), 1);
    assert_eq!(pipeline.active_decoder(), Some(replacement));
}

#[test]
fn completed_decoder_close_releases_pending_input_but_not_escaped_output_frames() {
    let mut pipeline = BrowserHevcPipeline::new(4, 4096);
    let decoder = configure(&mut pipeline, 0);
    let rendered = admit(&mut pipeline, decoder, 0);
    pipeline.on_frame_decoded(decoder, 0).unwrap();
    let pending = admit(&mut pipeline, decoder, 1);
    pipeline.on_decoder_reclaimed(decoder).unwrap();
    assert_eq!(pipeline.inflight_frames(), 2);

    let releases = pipeline.on_decoder_closed(decoder).unwrap();
    assert_eq!(releases.len(), 1);
    assert_eq!(releases[0].frame_id, 1);
    assert_eq!(releases[0].freed_bytes, pending.charged_bytes);
    assert_eq!(releases[0].remaining_inflight_frames, 1);
    assert_eq!(pipeline.inflight_bytes(), rendered.charged_bytes);
    assert_eq!(pipeline.on_decoder_closed(decoder).unwrap(), []);
    assert_eq!(
        pipeline.on_video_frame_closed(decoder, 1),
        Err(BrowserHevcError::UnknownFrameId)
    );
    pipeline.on_video_frame_closed(decoder, 0).unwrap();
    assert_eq!(pipeline.inflight_bytes(), 0);
}

#[test]
fn invalid_configuration_and_stale_transitions_leave_live_owner_intact() {
    let mut pipeline = BrowserHevcPipeline::new(4, 4096);
    let decoder = configure(&mut pipeline, 1);
    let first = admit(&mut pipeline, decoder, 0);
    for bad in [config(0), config(1)] {
        assert!(
            pipeline
                .admit_decoder_configuration(bad, ProtocolLimits::ABSOLUTE, 4, &hvcc())
                .is_err()
        );
    }
    assert!(
        pipeline
            .admit_decoder_configuration(config(2), ProtocolLimits::ABSOLUTE, 4, &[0])
            .is_err()
    );
    assert_eq!(pipeline.active_decoder(), Some(decoder));
    assert_eq!(pipeline.inflight_bytes(), first.charged_bytes);
    assert_eq!(
        pipeline.on_decoder_configured(decoder),
        Err(BrowserHevcError::InvalidDecoderState)
    );
    pipeline
        .admit_access_unit(decoder, 1, 2_000, false, &access_unit(&[P1]))
        .unwrap();
    assert_eq!(
        pipeline.on_frame_decoded(decoder, 1),
        Err(BrowserHevcError::AwaitingIdr)
    );
    pipeline.on_frame_decoded(decoder, 0).unwrap();
    pipeline.on_frame_decoded(decoder, 1).unwrap();
    pipeline
        .on_frame_presented(decoder, 1, 3_000, true)
        .unwrap();
    assert_eq!(
        pipeline.on_frame_presented(decoder, 0, 4_000, true),
        Err(BrowserHevcError::InvalidPresentationOrder)
    );
}

#[test]
fn negotiated_small_limits_are_not_silently_increased() {
    let charge = u64::try_from(access_unit(&[VPS, SPS, PPS, IDR]).len()).unwrap();
    for (frames, bytes) in [(0, 4096), (4, 0), (4, charge - 1)] {
        let mut pipeline = BrowserHevcPipeline::new(frames, bytes);
        let decoder = configure(&mut pipeline, 0);
        assert_eq!(
            pipeline.admit_access_unit(
                decoder,
                0,
                1_000,
                true,
                &access_unit(&[VPS, SPS, PPS, IDR])
            ),
            Err(BrowserHevcError::DecodeQueueFull)
        );
        assert_eq!(pipeline.inflight_frames(), 0);
    }
    let mut pipeline = BrowserHevcPipeline::new(usize::MAX, charge * 4);
    let decoder = configure(&mut pipeline, 0);
    for frame in 0..4 {
        admit(&mut pipeline, decoder, frame);
    }
    assert_eq!(
        pipeline.admit_access_unit(decoder, 4, 5_000, true, &access_unit(&[VPS, SPS, PPS, IDR])),
        Err(BrowserHevcError::DecodeQueueFull)
    );
}

#[test]
fn decoder_objects_are_bounded_even_before_any_output_exists() {
    let mut pipeline = BrowserHevcPipeline::new(4, 4096);
    let mut owners = Vec::new();
    for generation in 0..4 {
        owners.push(configure(&mut pipeline, generation));
    }
    assert_eq!(pipeline.retained_decoders(), 4);
    let active = pipeline.active_decoder();
    assert_eq!(
        pipeline.admit_decoder_configuration(config(4), ProtocolLimits::ABSOLUTE, 4, &hvcc()),
        Err(BrowserHevcError::DecoderCapacity)
    );
    assert_eq!(pipeline.active_decoder(), active);
    assert_eq!(pipeline.on_decoder_closed(owners[0]).unwrap(), []);
    assert!(!pipeline.contains_decoder(owners[0]));
    let replacement = configure(&mut pipeline, 4);
    assert_eq!(pipeline.retained_decoders(), 4);
    assert_eq!(
        pipeline.on_decoder_closed(owners[0]),
        Err(BrowserHevcError::StaleGeneration)
    );
    assert_eq!(pipeline.active_decoder(), Some(replacement));
}

#[test]
fn reconnect_requires_positive_decoder_cleanup_and_never_reuses_callback_tokens() {
    let mut pipeline = BrowserHevcPipeline::new(4, 4096);
    let old = configure(&mut pipeline, 0);
    assert_eq!(pipeline.inflight_frames(), 0);
    assert_eq!(
        pipeline.reset_session(),
        Err(BrowserHevcError::InvalidDecoderState)
    );
    pipeline.on_decoder_closed(old).unwrap();
    pipeline.reset_session().unwrap();
    let current = configure(&mut pipeline, 0);
    assert_ne!(current, old);
    assert!(current.serial > old.serial);
    admit(&mut pipeline, current, 0);
    assert_eq!(
        pipeline.on_frame_decoded(old, 0),
        Err(BrowserHevcError::StaleGeneration)
    );
    assert_eq!(
        pipeline.on_video_frame_closed(old, 0),
        Err(BrowserHevcError::UnknownFrameId)
    );
    assert_eq!(pipeline.inflight_frames(), 1);
    pipeline.on_frame_decoded(current, 0).unwrap();
}

fn binding() -> ControlBinding {
    ControlBinding {
        id: 1,
        host_boot: HostBootId::from_raw(10),
        os_session: OsSessionId::from_raw(20),
        remote_session: RemoteSessionId::from_raw(30),
    }
}

#[test]
fn credit_requires_complete_original_binding_and_receiver_direction() {
    for invalid in [
        ControlBinding { id: 0, ..binding() },
        ControlBinding {
            host_boot: HostBootId::from_raw(0),
            ..binding()
        },
        ControlBinding {
            os_session: OsSessionId::from_raw(0),
            ..binding()
        },
        ControlBinding {
            remote_session: RemoteSessionId::from_raw(0),
            ..binding()
        },
    ] {
        assert_eq!(
            BrowserCreditTracker::new(invalid, 2, ChannelRole::Video, 1, ProtocolLimits::ABSOLUTE)
                .unwrap_err(),
            BrowserCreditError::InvalidBindingOrEpoch
        );
    }
    assert_eq!(
        BrowserCreditTracker::new(
            binding(),
            2,
            ChannelRole::AudioUp,
            1,
            ProtocolLimits::ABSOLUTE
        )
        .unwrap_err(),
        BrowserCreditError::Wire(WireError::WrongRole)
    );
}

#[test]
fn credit_preflight_and_failed_release_leave_original_accounting_unchanged() {
    let limits = ProtocolLimits::ABSOLUTE;
    let mut credit =
        BrowserCreditTracker::new(binding(), 2, ChannelRole::Video, 1, limits).unwrap();
    let max_bytes = limits.per_viewer_compressed_bytes();
    credit.can_record_received(max_bytes).unwrap();
    assert_eq!(credit.outstanding_bytes(), 0);
    credit.record_received(max_bytes).unwrap();
    assert_eq!(
        credit.can_record_received(1),
        Err(BrowserCreditError::ResourceLimit)
    );
    assert_eq!(
        credit.record_received(1),
        Err(BrowserCreditError::ResourceLimit)
    );
    assert_eq!(credit.outstanding_bytes(), max_bytes);
    assert_eq!(
        credit.record_freed_and_encode(max_bytes, 1, 1),
        Err(BrowserCreditError::InvalidQueueUsage)
    );
    assert_eq!(credit.released_frames(), 0);
    let encoded = credit.record_freed_and_encode(max_bytes, 0, 0).unwrap();
    let report = receiver_pressure::decode(
        &encoded,
        binding(),
        &limits,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(report.released_bytes, max_bytes);
    assert_eq!(report.released_frames, 1);
    credit.record_received(50).unwrap();
    credit.record_received(75).unwrap();
    credit.record_freed_and_encode(50, 75, 1).unwrap();
    assert_eq!(credit.outstanding_frames(), 1);
    credit.record_freed_and_encode(75, 0, 0).unwrap();
    assert_eq!(credit.released_frames(), 3);
}
