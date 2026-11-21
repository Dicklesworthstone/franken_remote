use fr_core::{
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, InputLeaseId,
        InputTicketId, OsSessionId, RemoteSessionId, ViewportMappingGeneration,
    },
    limits::ProtocolLimits,
};
use fr_media::{
    config::{CodecConfiguration, CodedGeometry, ColorInfo, GopPolicy},
    hevc::{HevcError, HevcGuard},
};
use fr_transport::wss::{
    ChannelGeneration, CreditUpdate, ReceiverCreditWindow, WssError, WssProfile,
};
use fr_web::{
    BrowserChannelKind, BrowserCreditError, BrowserCreditTracker, BrowserDecoderStage,
    BrowserHevcError, BrowserLifecycleEvent, BrowserProtocolError, BrowserSessionCore,
    BrowserTransportProfile, encode_attach_message, encode_auth_message, encode_ticket_grant,
    parse_attach_message, parse_auth_message, parse_ticket_grant,
};
use fr_wire::{
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    receiver_pressure::{self, ChannelRole},
};

// Real FFmpeg 7.1.5 / libx265 Main 8-bit 4:2:0 BT.709 320x240 fixtures (matching fr-media tests).
const VPS: &str = "40010c01ffff01600000030090000003000003003cba0240";
const SPS: &str =
    "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04";
const UNSPECIFIED_COLOR_SPS: &str =
    "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a040402080000003008000000f04";
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

fn length_prefixed_au(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        let len = u32::try_from(nal.len()).unwrap();
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

fn baseline_config() -> CodecConfiguration {
    CodecConfiguration::new_baseline(
        CodecConfigurationGeneration::INITIAL,
        CodedGeometry::new(&ProtocolLimits::ABSOLUTE, 320, 240, 320, 240, 2).unwrap(),
        ColorInfo::sdr_bt709(),
        GopPolicy::baseline_for_frame_rate(30).unwrap(),
    )
    .unwrap()
}

fn canonical_hvcc() -> Vec<u8> {
    let mut guard = HevcGuard::new(baseline_config(), ProtocolLimits::ABSOLUTE, 4).unwrap();
    let idr = length_prefixed_au(&[&hex(VPS), &hex(SPS), &hex(PPS), &hex(IDR)]);
    guard.validate_length_prefixed(&idr, true).unwrap();
    guard.decoder_record().unwrap().bytes().to_vec()
}

fn control_binding() -> ControlBinding {
    ControlBinding {
        id: 1,
        host_boot: HostBootId::from_raw(10),
        os_session: OsSessionId::from_raw(20),
        remote_session: RemoteSessionId::from_raw(30),
    }
}

#[test]
fn non_circular_startup_credit_return_and_observation_control_separation() {
    let limits = ProtocolLimits::ABSOLUTE;
    let profile = BrowserTransportProfile::from_wss(WssProfile::new(false));
    assert!(profile.is_degraded());
    assert_eq!(profile.diagnostic_label(), "wss_degraded_tcp_hol_dedicated");

    let mut core = BrowserSessionCore::new(profile, limits);
    core.begin_connect(1_000).unwrap();
    core.on_session_opened(control_binding(), 2, 7, 2_000)
        .unwrap();

    // 1. Configure decoder from exact canonical hvcC (SPS-derived codec string, never hard-coded).
    let hvcc = canonical_hvcc();
    let webcodecs = core
        .admit_decoder_configuration(baseline_config(), 4, &hvcc)
        .unwrap();
    assert!(webcodecs.codec_string.starts_with("hvc1.1.6.L60."));
    assert_eq!(webcodecs.coded_width, 320);
    assert_eq!(webcodecs.coded_height, 240);
    assert_eq!(
        core.snapshot().decoder_stage,
        BrowserDecoderStage::AwaitingConfig
    );

    core.on_decoder_configured(CodecConfigurationGeneration::INITIAL)
        .unwrap();
    assert_eq!(
        core.snapshot().decoder_stage,
        BrowserDecoderStage::Configured { generation: 1 }
    );
    // API configuration is NOT first frame and cannot request control.
    assert!(!core.snapshot().can_request_control);
    assert!(core.request_control(2_500).is_err());

    // 2. Delta frame before verified IDR must be refused.
    let p1 = length_prefixed_au(&[&hex(P1)]);
    assert_eq!(
        core.admit_video_chunk(
            ChannelGeneration(1),
            CodecConfigurationGeneration::INITIAL,
            1,
            3_000,
            false,
            &p1,
        ),
        Err(BrowserHevcError::AwaitingIdr)
    );

    // 3. Verified IDR passes HevcGuard::prepare_hvc1 (strips in-band parameter sets).
    let idr = length_prefixed_au(&[&hex(VPS), &hex(SPS), &hex(PPS), &hex(IDR)]);
    let chunk = core
        .admit_video_chunk(
            ChannelGeneration(1),
            CodecConfigurationGeneration::INITIAL,
            1,
            3_000,
            true,
            &idr,
        )
        .unwrap();
    assert!(chunk.is_key);
    assert!(chunk.hvc1_bytes.len() < idr.len());
    assert_eq!(core.snapshot().inflight_frames, 1);

    // Host credit window consumes the frame; decode/present alone MUST NOT return credit!
    let mut host_window = ReceiverCreditWindow::new(7, 4096, 4).unwrap();
    host_window.try_reserve(chunk.charged_bytes).unwrap();
    assert_eq!(host_window.outstanding_frames(), 1);

    core.on_frame_decoded(CodecConfigurationGeneration::INITIAL, 1)
        .unwrap();
    assert_eq!(
        core.snapshot().decoder_stage,
        BrowserDecoderStage::FirstFrameDecoded {
            generation: 1,
            frame_id: 1,
        }
    );
    // Receiving/decoding first frame NEVER grants control or frees credit!
    assert!(!core.snapshot().is_controlling);
    assert_eq!(core.snapshot().released_frames, 0);

    core.on_frame_presented(CodecConfigurationGeneration::INITIAL, 1, 3_500)
        .unwrap();
    assert!(!core.snapshot().is_controlling);
    assert!(core.snapshot().can_request_control);
    assert_eq!(core.snapshot().released_frames, 0);

    // 4. ONLY VideoFrame.close() emits 0x0081 ReceiverPressure and unlocks host send credit!
    let pressure_bytes = core.on_video_frame_closed(1).unwrap().unwrap();
    let (decoded_binding, decoded_report) = receiver_pressure::decode(
        &pressure_bytes,
        &limits,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(decoded_binding, control_binding());
    assert_eq!(decoded_report.credit_epoch, 7);
    assert_eq!(decoded_report.released_frames, 1);
    assert_eq!(decoded_report.released_bytes, chunk.charged_bytes);

    assert_eq!(
        host_window
            .apply_wire_pressure(2, ChannelRole::Video, &decoded_report)
            .unwrap(),
        CreditUpdate::Applied
    );
    assert_eq!(host_window.outstanding_frames(), 0);
    assert_eq!(host_window.outstanding_bytes(), 0);

    // 5. Explicit control request after fresh visible presentation.
    core.request_control(4_000).unwrap();
    core.on_control_granted(InputLeaseId::from_raw(99), InputTicketId::from_raw(100))
        .unwrap();
    assert!(core.snapshot().is_controlling);
    assert!(
        core.authorize_input(
            DisplayGeometryGeneration::INITIAL,
            ViewportMappingGeneration::INITIAL
        )
        .is_ok()
    );

    // Stale viewport generation is rejected immediately.
    let stale_vp = ViewportMappingGeneration::INITIAL.next().unwrap();
    assert!(
        core.authorize_input(DisplayGeometryGeneration::INITIAL, stale_vp)
            .is_err()
    );
}

#[test]
fn hidden_tab_bfcache_and_inband_parameter_change_fence_authority_and_decoder() {
    let limits = ProtocolLimits::ABSOLUTE;
    let mut core = BrowserSessionCore::new(
        BrowserTransportProfile::from_wss(WssProfile::new(false)),
        limits,
    );
    core.begin_connect(1_000).unwrap();
    core.on_session_opened(control_binding(), 2, 1, 2_000)
        .unwrap();
    core.admit_decoder_configuration(baseline_config(), 4, &canonical_hvcc())
        .unwrap();
    core.on_decoder_configured(CodecConfigurationGeneration::INITIAL)
        .unwrap();

    let idr = length_prefixed_au(&[&hex(VPS), &hex(SPS), &hex(PPS), &hex(IDR)]);
    core.admit_video_chunk(
        ChannelGeneration(1),
        CodecConfigurationGeneration::INITIAL,
        1,
        3_000,
        true,
        &idr,
    )
    .unwrap();
    core.on_frame_decoded(CodecConfigurationGeneration::INITIAL, 1)
        .unwrap();
    core.on_frame_presented(CodecConfigurationGeneration::INITIAL, 1, 3_200)
        .unwrap();
    core.on_video_frame_closed(1).unwrap();

    // Grant control.
    core.request_control(4_000).unwrap();
    core.on_control_granted(InputLeaseId::from_raw(5), InputTicketId::from_raw(6))
        .unwrap();
    assert!(core.snapshot().is_controlling);

    // Tab hidden MUST suspend control immediately and forbid hidden presentation claims.
    core.on_lifecycle_event(BrowserLifecycleEvent::TabHidden, 5_000);
    assert!(!core.snapshot().is_controlling);
    assert!(!core.snapshot().visible);
    assert!(
        core.authorize_input(
            DisplayGeometryGeneration::INITIAL,
            ViewportMappingGeneration::INITIAL
        )
        .is_err()
    );

    let p1 = length_prefixed_au(&[&hex(P1)]);
    core.admit_video_chunk(
        ChannelGeneration(1),
        CodecConfigurationGeneration::INITIAL,
        2,
        5_100,
        false,
        &p1,
    )
    .unwrap();
    assert_eq!(
        core.on_frame_presented(CodecConfigurationGeneration::INITIAL, 2, 5_200),
        Err(BrowserHevcError::HiddenPresentationForbidden)
    );
    core.on_video_frame_closed(2).unwrap();

    // Unannounced in-band parameter-set change MUST be rejected by HevcGuard.
    let rogue_sps_idr =
        length_prefixed_au(&[&hex(VPS), &hex(UNSPECIFIED_COLOR_SPS), &hex(PPS), &hex(IDR)]);
    assert!(matches!(
        core.admit_video_chunk(
            ChannelGeneration(1),
            CodecConfigurationGeneration::INITIAL,
            3,
            5_500,
            true,
            &rogue_sps_idr,
        ),
        Err(BrowserHevcError::Hevc(
            HevcError::ChangedParameterSet | HevcError::ColorMismatch
        ))
    ));

    // Bfcache restoration fences decoder into NeedsRecovery.
    core.on_lifecycle_event(BrowserLifecycleEvent::BfcacheRestore, 6_000);
    assert_eq!(
        core.snapshot().decoder_stage,
        BrowserDecoderStage::NeedsRecovery { generation: 1 }
    );
    let p2 = length_prefixed_au(&[&hex(P2)]);
    assert_eq!(
        core.admit_video_chunk(
            ChannelGeneration(1),
            CodecConfigurationGeneration::INITIAL,
            4,
            6_100,
            false,
            &p2,
        ),
        Err(BrowserHevcError::NotConfigured)
    );
}

#[test]
fn decode_queue_ceiling_stale_channel_gen_and_credit_bounds_enforced() {
    let limits = ProtocolLimits::ABSOLUTE;
    let mut core = BrowserSessionCore::new(
        BrowserTransportProfile::from_wss(WssProfile::new(false)),
        limits,
    );
    core.begin_connect(100).unwrap();
    core.on_session_opened(control_binding(), 2, 3, 200)
        .unwrap();
    core.admit_decoder_configuration(baseline_config(), 4, &canonical_hvcc())
        .unwrap();
    core.on_decoder_configured(CodecConfigurationGeneration::INITIAL)
        .unwrap();

    let idr = length_prefixed_au(&[&hex(VPS), &hex(SPS), &hex(PPS), &hex(IDR)]);
    // Fill bounded decode queue (max 4 in-flight frames).
    for id in 1..=4 {
        core.admit_video_chunk(
            ChannelGeneration(1),
            CodecConfigurationGeneration::INITIAL,
            id,
            id * 1000,
            true,
            &idr,
        )
        .unwrap();
    }
    // 5th unclosed frame MUST be refused.
    assert_eq!(
        core.admit_video_chunk(
            ChannelGeneration(1),
            CodecConfigurationGeneration::INITIAL,
            5,
            5000,
            true,
            &idr,
        ),
        Err(BrowserHevcError::DecodeQueueFull)
    );

    // Closing one VideoFrame frees a slot.
    core.on_video_frame_closed(1).unwrap();
    core.admit_video_chunk(
        ChannelGeneration(1),
        CodecConfigurationGeneration::INITIAL,
        5,
        5000,
        true,
        &idr,
    )
    .unwrap();

    // Channel replacement fences old channel generation immediately.
    let gen2 = core.replace_video_channel().unwrap();
    assert_eq!(gen2, ChannelGeneration(2));
    assert_eq!(
        core.admit_video_chunk(
            ChannelGeneration(1),
            CodecConfigurationGeneration::INITIAL,
            6,
            6000,
            true,
            &idr,
        ),
        Err(BrowserHevcError::StaleGeneration)
    );
    core.complete_video_channel_close(ChannelGeneration(1));

    // Credit tracker rejects releasing more than received and zero binding/epoch.
    assert_eq!(
        BrowserCreditTracker::new(control_binding(), 0, ChannelRole::Video, 1, limits).unwrap_err(),
        BrowserCreditError::InvalidBindingOrEpoch
    );
    let mut tracker =
        BrowserCreditTracker::new(control_binding(), 2, ChannelRole::Video, 1, limits).unwrap();
    assert_eq!(
        tracker.record_freed_and_encode(100, 0, 0),
        Err(BrowserCreditError::ReleaseExceedsReceived)
    );
    tracker.record_received(100).unwrap();
    tracker.record_freed_and_encode(100, 0, 0).unwrap();
    assert_eq!(
        tracker.record_freed_and_encode(1, 0, 0),
        Err(BrowserCreditError::ReleaseExceedsReceived)
    );

    // Exhaust channel replacement closing-set ceiling to confirm flood throttling.
    for _ in 0..4 {
        let _ = core.replace_video_channel().unwrap();
    }
    assert!(matches!(
        core.replace_video_channel(),
        Err(WssError::ResetFloodThrottled(4))
    ));
}

#[test]
fn binary_auth_attach_and_ticket_frames_roundtrip_and_reject_malformed() {
    let nonce = [0xAA; 32];
    let auth = encode_auth_message(&nonce);
    assert_eq!(parse_auth_message(&auth), Ok(nonce));
    assert_eq!(
        parse_auth_message(&auth[..35]),
        Err(BrowserProtocolError::InvalidLength)
    );
    let mut bad_auth = auth;
    bad_auth[0] = b'X';
    assert_eq!(
        parse_auth_message(&bad_auth),
        Err(BrowserProtocolError::MalformedMagic)
    );

    let sid = RemoteSessionId::from_raw(0x1234_5678);
    let ticket = [0x55; 32];
    let attach = encode_attach_message(sid, BrowserChannelKind::Video, &ticket).unwrap();
    assert_eq!(
        parse_attach_message(&attach),
        Ok((sid, BrowserChannelKind::Video, ticket))
    );
    assert_eq!(
        encode_attach_message(
            RemoteSessionId::from_raw(0),
            BrowserChannelKind::Video,
            &ticket
        ),
        Err(BrowserProtocolError::ZeroSessionId)
    );

    let grant = encode_ticket_grant(sid, BrowserChannelKind::AudioDown, &ticket).unwrap();
    assert_eq!(
        parse_ticket_grant(&grant),
        Ok((sid, BrowserChannelKind::AudioDown, ticket))
    );
    let mut bad_role = grant;
    bad_role[4] = 99;
    assert_eq!(
        parse_ticket_grant(&bad_role),
        Err(BrowserProtocolError::InvalidChannelRole)
    );
}
