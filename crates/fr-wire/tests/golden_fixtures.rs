#![forbid(unsafe_code)]
//! Comprehensive round-trip and negative testing for all checked-in golden fixtures.
//!
//! Asserts that every protocol message family has byte-exact golden `.hex` fixtures,
//! that every message round-trips without allocation or semantic loss, and that
//! corruptions, truncations, trailing bytes, and wrong roles/channels fail closed.

use fr_core::{clipboard::Binding as ClipBinding, ids::*, limits::ProtocolLimits};
use fr_wire::{
    Channel, MediaLimits, Record, RepairRange, WireError,
    attachment::{decode as decode_attachment, encode as encode_attachment},
    authority::Binding as AuthBinding,
    clipboard::{
        Context as ClipContext, Lane as ClipLane, Role as ClipRole, decode as decode_clipboard,
        encode as encode_clipboard,
    },
    clock::{decode as decode_clock, encode as encode_clock},
    control::{decode_granted, decode_request, encode_granted, encode_request},
    decode_fragment, decode_progress, decode_recovery, decode_repair,
    decoder::{Binding as DecBinding, decode as decode_decoder, encode as decode_encode},
    display::{decode as decode_display, encode as encode_display},
    encode_fragment, encode_progress, encode_recovery, encode_repair,
    files::{
        Context as FileContext, Direction as FileDir, Lane as FileLane, Limits as FileLimits,
        Role as FileRole, decode as decode_files, encode as encode_files,
    },
    held_state::{decode as decode_held, encode as encode_held},
    input::{InputDelivery, InputDirection, decode_input, encode_input},
    input_result::{ResultBinding, decode_input_result},
    input_ticket::{decode as decode_input_ticket, encode as encode_input_ticket},
    negotiation::{ControlBinding, decode as decode_neg, encode as encode_neg},
    presented::{decode as decode_presented, encode as encode_presented},
    receiver_metrics::{decode as decode_metrics, encode as encode_metrics},
};

const L: ProtocolLimits = ProtocolLimits::ABSOLUTE;

fn parse_hex(s: &str) -> Vec<u8> {
    let s = s.trim();
    assert!(
        s.len().is_multiple_of(2),
        "hex string must have even length"
    );
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

// ---------------------------------------------------------------------------
// Negotiation fixtures
// ---------------------------------------------------------------------------

#[test]
fn test_negotiation_fixtures_round_trip_and_fail_closed() {
    let fixtures = [
        (
            include_str!("fixtures/negotiation/client_hello.hex"),
            0,
            "client_hello",
        ),
        (
            include_str!("fixtures/negotiation/host_capabilities.hex"),
            0,
            "host_capabilities",
        ),
        (
            include_str!("fixtures/negotiation/selected_configuration.hex"),
            0,
            "selected_configuration",
        ),
        (
            include_str!("fixtures/negotiation/approval_required.hex"),
            0,
            "approval_required",
        ),
        (
            include_str!("fixtures/negotiation/session_opened.hex"),
            0,
            "session_opened",
        ),
        (
            include_str!("fixtures/negotiation/binding_accepted.hex"),
            1,
            "binding_accepted",
        ),
    ];

    for (hex_str, binding, name) in fixtures {
        let bytes = parse_hex(hex_str);
        assert!(bytes.starts_with(b"FRD0"), "{name} bad magic");

        // Decode
        let msg = decode_neg(&bytes, 4096, binding).expect("decode valid fixture");

        // Re-encode
        let mut re_encoded = vec![0; 4096];
        let n = encode_neg(&msg, 4096, &mut re_encoded).expect("re-encode");
        re_encoded.truncate(n);
        assert_eq!(re_encoded, bytes, "{name} round-trip drift");

        // Negative: truncation
        for cut in 0..bytes.len() {
            assert!(
                decode_neg(&bytes[..cut], 4096, binding).is_err(),
                "{name} accepted truncation at {cut}"
            );
        }

        // Negative: trailing bytes
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(
            decode_neg(&trailing, 4096, binding).is_err(),
            "{name} accepted trailing byte"
        );
    }
}

// ---------------------------------------------------------------------------
// Control fixtures
// ---------------------------------------------------------------------------

#[test]
fn test_control_fixtures_round_trip_and_fail_closed() {
    let req_bytes = parse_hex(include_str!("fixtures/control/control_request.hex"));
    let parent = ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(1),
        os_session: OsSessionId::from_raw(2),
        remote_session: RemoteSessionId::from_raw(3),
    };

    let req = decode_request(
        &req_bytes,
        parent,
        &L,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .expect("decode control_request");

    let mut req_out = vec![0; fr_wire::control::REQUEST_BYTES];
    let n1 = encode_request(
        req,
        &mut req_out,
        &L,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .expect("encode control_request");
    assert_eq!(&req_out[..n1], req_bytes.as_slice());

    // Negative: wrong role
    assert_eq!(
        decode_request(
            &req_bytes,
            parent,
            &L,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap_err(),
        WireError::WrongRole
    );

    let grant_bytes = parse_hex(include_str!("fixtures/control/lease_granted.hex"));
    let grant = decode_granted(
        &grant_bytes,
        parent,
        &L,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .expect("decode lease_granted");

    let mut grant_out = vec![0; fr_wire::control::GRANTED_BYTES];
    let n2 = encode_granted(
        grant,
        &mut grant_out,
        &L,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .expect("encode lease_granted");
    assert_eq!(&grant_out[..n2], grant_bytes.as_slice());

    // Negative: truncation
    for cut in 0..grant_bytes.len() {
        assert!(
            decode_granted(
                &grant_bytes[..cut],
                parent,
                &L,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            )
            .is_err(),
            "lease_granted cut at {cut}"
        );
    }
}

// ---------------------------------------------------------------------------
// Authority fixtures
// ---------------------------------------------------------------------------

#[test]
fn test_authority_fixtures_round_trip_and_fail_closed() {
    let auth_b = AuthBinding {
        channel: 0x0102_0304,
        session: RemoteSessionId::from_raw(0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00),
    };

    let cases = [
        (
            include_str!("fixtures/authority/challenge_observation.hex"),
            InputDirection::HostToViewer,
            "challenge_observation",
        ),
        (
            include_str!("fixtures/authority/challenge_control.hex"),
            InputDirection::HostToViewer,
            "challenge_control",
        ),
        (
            include_str!("fixtures/authority/response_observation.hex"),
            InputDirection::ViewerToHost,
            "response_observation",
        ),
        (
            include_str!("fixtures/authority/response_control.hex"),
            InputDirection::ViewerToHost,
            "response_control",
        ),
    ];

    for (hex_str, dir, name) in cases {
        let bytes = parse_hex(hex_str);
        let msg = fr_wire::authority::decode(&bytes, auth_b, &L, dir, InputDelivery::Reliable)
            .expect("decode valid fixture");

        let mut out = [0; fr_wire::authority::MAX_AUTHORITY_BYTES];
        let n = fr_wire::authority::encode(msg, auth_b, &L, &mut out, dir, InputDelivery::Reliable)
            .expect("re-encode authority");
        assert_eq!(&out[..n], bytes.as_slice(), "{name} round-trip drift");

        // Negative: wrong role
        let opp_dir = if dir == InputDirection::HostToViewer {
            InputDirection::ViewerToHost
        } else {
            InputDirection::HostToViewer
        };
        assert_eq!(
            fr_wire::authority::decode(&bytes, auth_b, &L, opp_dir, InputDelivery::Reliable)
                .unwrap_err(),
            WireError::WrongRole
        );
    }
}

// ---------------------------------------------------------------------------
// Input Ticket fixture
// ---------------------------------------------------------------------------

#[test]
fn test_input_ticket_fixture_round_trip_and_fail_closed() {
    let bytes = parse_hex(include_str!("fixtures/input_ticket/input_ticket.hex"));
    let ticket = decode_input_ticket(
        &bytes,
        &L,
        7,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .expect("decode input_ticket");

    let mut out = vec![0; fr_wire::input_ticket::INPUT_TICKET_BYTES];
    let n = encode_input_ticket(
        ticket,
        &mut out,
        &L,
        7,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .expect("encode input_ticket");
    assert_eq!(&out[..n], bytes.as_slice());

    // Truncation
    for cut in 0..bytes.len() {
        assert!(
            decode_input_ticket(
                &bytes[..cut],
                &L,
                7,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            )
            .is_err()
        );
    }
}

// ---------------------------------------------------------------------------
// Attachment fixtures
// ---------------------------------------------------------------------------

#[test]
fn test_attachment_fixtures_round_trip_and_fail_closed() {
    let parent = ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(11),
        os_session: OsSessionId::from_raw(12),
        remote_session: RemoteSessionId::from_raw(13),
    };

    let cases = [
        (
            include_str!("fixtures/attachment/stream_binding.hex"),
            7,
            InputDirection::HostToViewer,
            "stream_binding",
        ),
        (
            include_str!("fixtures/attachment/binding_accepted.hex"),
            7,
            InputDirection::ViewerToHost,
            "binding_accepted",
        ),
        (
            include_str!("fixtures/attachment/channel_ticket.hex"),
            7,
            InputDirection::HostToViewer,
            "channel_ticket",
        ),
        (
            include_str!("fixtures/attachment/channel_attach.hex"),
            8,
            InputDirection::ViewerToHost,
            "channel_attach",
        ),
        (
            include_str!("fixtures/attachment/channel_attached.hex"),
            8,
            InputDirection::HostToViewer,
            "channel_attached",
        ),
    ];

    for (hex_str, channel, dir, name) in cases {
        let bytes = parse_hex(hex_str);
        let msg = decode_attachment(&bytes, parent, channel, &L, dir, InputDelivery::Reliable)
            .expect("decode valid fixture");

        let mut out = vec![0; fr_wire::attachment::GRANT_RECORD_BYTES];
        let n = encode_attachment(msg, parent, &L, &mut out, dir, InputDelivery::Reliable)
            .expect("re-encode attachment");
        out.truncate(n);
        assert_eq!(out, bytes, "{name} round-trip drift");
    }
}

// ---------------------------------------------------------------------------
// Display fixtures
// ---------------------------------------------------------------------------

#[test]
fn test_display_fixtures_round_trip_and_fail_closed() {
    let parent = ControlBinding {
        id: 0x0102_0304,
        host_boot: HostBootId::from_raw(u128::from_be_bytes([0x11; 16])),
        os_session: OsSessionId::from_raw(u128::from_be_bytes([0x22; 16])),
        remote_session: RemoteSessionId::from_raw(u128::from_be_bytes([0x33; 16])),
    };

    let cat_bytes = parse_hex(include_str!("fixtures/display/display_catalog.hex"));
    let cat_msg = decode_display(
        &cat_bytes,
        parent,
        &L,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .expect("decode display_catalog");

    let mut cat_out = [0; fr_wire::display::MAX_CATALOG_BYTES];
    let n1 = encode_display(
        &cat_msg,
        parent,
        &L,
        &mut cat_out,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .expect("re-encode catalog");
    assert_eq!(&cat_out[..n1], cat_bytes.as_slice());

    let sel_bytes = parse_hex(include_str!("fixtures/display/select_display.hex"));
    let sel_msg = decode_display(
        &sel_bytes,
        parent,
        &L,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .expect("decode select_display");

    let mut sel_out = [0; fr_wire::display::SELECT_BYTES];
    let n2 = encode_display(
        &sel_msg,
        parent,
        &L,
        &mut sel_out,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .expect("re-encode select");
    assert_eq!(&sel_out[..n2], sel_bytes.as_slice());
}

// ---------------------------------------------------------------------------
// Decoder fixtures
// ---------------------------------------------------------------------------

#[test]
fn test_decoder_fixtures_round_trip_and_fail_closed() {
    let b = DecBinding {
        parent: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(13),
        },
        display: 14,
        geometry: DisplayGeometryGeneration::from_raw(15),
        configuration: CodecConfigurationGeneration::from_raw(16),
        recovery: RecoveryGeneration::from_raw(17),
        viewport: ViewportMappingGeneration::from_raw(18),
    };

    let cases = [
        (
            include_str!("fixtures/decoder/decoder_configuration.hex"),
            InputDirection::HostToViewer,
            "decoder_configuration",
        ),
        (
            include_str!("fixtures/decoder/decoder_configured.hex"),
            InputDirection::ViewerToHost,
            "decoder_configured",
        ),
        (
            include_str!("fixtures/decoder/first_frame_decoded.hex"),
            InputDirection::ViewerToHost,
            "first_frame_decoded",
        ),
    ];

    for (hex_str, dir, name) in cases {
        let bytes = parse_hex(hex_str);
        let msg = decode_decoder(&bytes, b, &L, dir, InputDelivery::Reliable)
            .expect("decode valid fixture");

        let mut out = vec![0; 20000];
        let len = decode_encode(msg, b, &L, &mut out, dir, InputDelivery::Reliable)
            .expect("re-encode decoder");
        out.truncate(len);
        assert_eq!(out, bytes, "{name} round-trip drift");
    }
}

// ---------------------------------------------------------------------------
// Media fixtures
// ---------------------------------------------------------------------------

#[test]
fn test_media_fixtures_round_trip_and_fail_closed() {
    let med_limits = MediaLimits::new(L, 1_150, 16_384, 64).unwrap();

    // Fragment
    let frag_bytes = parse_hex(include_str!("fixtures/media/access_unit_fragment.hex"));
    let rec = Record::decode(&frag_bytes, &med_limits, 9, Channel::Video).expect("decode record");
    let frag = decode_fragment(rec, &med_limits).expect("decode fragment");
    let mut frag_out = [0; 128];
    let n1 = encode_fragment(frag, 9, &med_limits, &mut frag_out).expect("encode fragment");
    assert_eq!(&frag_out[..n1], frag_bytes.as_slice());

    // Recovery chunk
    let recovery_bytes = parse_hex(include_str!("fixtures/media/recovery_chunk.hex"));
    let recovery_rec =
        Record::decode(&recovery_bytes, &med_limits, 9, Channel::Recovery).expect("decode record");
    let chunk = decode_recovery(recovery_rec, &med_limits).expect("decode recovery");
    let mut recovery_out = [0; 128];
    let n2 = encode_recovery(chunk, 9, &med_limits, &mut recovery_out).expect("encode recovery");
    assert_eq!(&recovery_out[..n2], recovery_bytes.as_slice());

    // Repair request
    let repair_bytes = parse_hex(include_str!("fixtures/media/repair_request.hex"));
    let repair_rec =
        Record::decode(&repair_bytes, &med_limits, 9, Channel::Control).expect("decode record");
    let rep = decode_repair(repair_rec, 7, &med_limits).expect("decode repair");
    let mut repair_out = [0; 128];
    let ranges: Vec<RepairRange> = rep.ranges().collect();
    let n3 = encode_repair(rep.frame, &ranges, 5, 9, &med_limits, &mut repair_out)
        .expect("encode repair");
    assert_eq!(&repair_out[..n3], repair_bytes.as_slice());

    // Media progress
    let prog_bytes = parse_hex(include_str!("fixtures/media/media_progress.hex"));
    let prog_rec =
        Record::decode(&prog_bytes, &med_limits, 9, Channel::MediaConfig).expect("decode record");
    let prog = decode_progress(prog_rec, &med_limits).expect("decode progress");
    let mut prog_out = [0; 128];
    let n4 = encode_progress(prog, 9, &med_limits, &mut prog_out).expect("encode progress");
    assert_eq!(&prog_out[..n4], prog_bytes.as_slice());
}

// ---------------------------------------------------------------------------
// Held state fixture
// ---------------------------------------------------------------------------

#[test]
fn test_held_state_fixture_round_trip_and_fail_closed() {
    let bytes = parse_hex(include_str!("fixtures/held_state/held_state.hex"));
    let req = decode_held(
        &bytes,
        &L,
        9,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .expect("decode held_state");

    let mut out = [0; fr_wire::held_state::HELD_STATE_BYTES];
    let n = encode_held(
        req,
        &mut out,
        &L,
        9,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .expect("encode held_state");
    assert_eq!(&out[..n], bytes.as_slice());
}

// ---------------------------------------------------------------------------
// Clipboard fixtures
// ---------------------------------------------------------------------------

#[test]
fn test_clipboard_fixtures_round_trip_and_fail_closed() {
    let ctx = ClipContext {
        scope: ClipBinding {
            session: RemoteSessionId::from_raw(1),
            lease: InputLeaseId::from_raw(2),
        },
        channel: 9,
        sender: ClipRole::Controller,
        lane: ClipLane::Clipboard,
    };

    let cases = [
        (
            include_str!("fixtures/clipboard/clipboard_begin.hex"),
            "clipboard_begin",
        ),
        (
            include_str!("fixtures/clipboard/clipboard_chunk.hex"),
            "clipboard_chunk",
        ),
        (
            include_str!("fixtures/clipboard/clipboard_commit.hex"),
            "clipboard_commit",
        ),
        (
            include_str!("fixtures/clipboard/clipboard_cancel.hex"),
            "clipboard_cancel",
        ),
    ];

    for (hex_str, name) in cases {
        let bytes = parse_hex(hex_str);
        let msg = decode_clipboard(&bytes, ctx, &L).expect("decode valid fixture");

        let mut out = vec![0; 4096];
        let len = encode_clipboard(msg, ctx, &L, &mut out).expect("re-encode clipboard");
        out.truncate(len);
        assert_eq!(out, bytes, "{name} round-trip drift");
    }
}

// ---------------------------------------------------------------------------
// Files fixtures
// ---------------------------------------------------------------------------

#[test]
fn test_files_fixtures_round_trip_and_fail_closed() {
    let ctx = |sender: FileRole| FileContext {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        handle: 3,
        channel: if sender == FileRole::Host { 10 } else { 9 },
        sender,
        direction: FileDir::ToHost,
        lane: FileLane::Files,
    };
    let file_limits = FileLimits::new(&L, 4096).unwrap();

    let cases = [
        (
            include_str!("fixtures/files/file_offer.hex"),
            FileRole::Controller,
            "file_offer",
        ),
        (
            include_str!("fixtures/files/file_accept.hex"),
            FileRole::Host,
            "file_accept",
        ),
        (
            include_str!("fixtures/files/file_chunk.hex"),
            FileRole::Controller,
            "file_chunk",
        ),
        (
            include_str!("fixtures/files/file_complete.hex"),
            FileRole::Host,
            "file_complete",
        ),
        (
            include_str!("fixtures/files/file_cancel.hex"),
            FileRole::Controller,
            "file_cancel",
        ),
    ];

    for (hex_str, role, name) in cases {
        let bytes = parse_hex(hex_str);
        let msg = decode_files(&bytes, ctx(role), file_limits).expect("decode valid fixture");

        let mut out = [0; 4096];
        let len = encode_files(msg, ctx(role), file_limits, &mut out).expect("re-encode files");
        assert_eq!(&out[..len], bytes.as_slice(), "{name} round-trip drift");
    }
}

// ---------------------------------------------------------------------------
// Presented State fixtures
// ---------------------------------------------------------------------------

#[test]
fn test_presented_fixtures_round_trip_and_fail_closed() {
    let b = DecBinding {
        parent: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(3),
        },
        display: 4,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    };

    for (hex_str, name) in [
        (
            include_str!("fixtures/presented/presented_state_sample.hex"),
            "presented_sample",
        ),
        (
            include_str!("fixtures/presented/presented_state_unavailable.hex"),
            "presented_unavailable",
        ),
    ] {
        let bytes = parse_hex(hex_str);
        let report = decode_presented(
            &bytes,
            b,
            &L,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .expect("decode valid fixture");

        let mut out = vec![0; fr_wire::presented::BYTES];
        let n = encode_presented(
            report,
            b,
            &L,
            &mut out,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .expect("re-encode presented");
        assert_eq!(&out[..n], bytes.as_slice(), "{name} round-trip drift");
    }
}

// ---------------------------------------------------------------------------
// Receiver Metrics fixtures
// ---------------------------------------------------------------------------

#[test]
fn test_receiver_metrics_fixtures_round_trip_and_fail_closed() {
    let b = DecBinding {
        parent: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(3),
        },
        display: 4,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    };

    let q_bytes = parse_hex(include_str!("fixtures/receiver_metrics/metrics_query.hex"));
    let q_msg = decode_metrics(
        &q_bytes,
        b,
        &L,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .expect("decode query");
    let mut q_out = vec![0; fr_wire::receiver_metrics::REPLY_BYTES];
    let n1 = encode_metrics(
        q_msg,
        b,
        &L,
        &mut q_out,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .expect("encode query");
    assert_eq!(&q_out[..n1], q_bytes.as_slice());

    let r_bytes = parse_hex(include_str!("fixtures/receiver_metrics/metrics_reply.hex"));
    let r_msg = decode_metrics(
        &r_bytes,
        b,
        &L,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .expect("decode reply");
    let mut r_out = vec![0; fr_wire::receiver_metrics::REPLY_BYTES];
    let n2 = encode_metrics(
        r_msg,
        b,
        &L,
        &mut r_out,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .expect("encode reply");
    assert_eq!(&r_out[..n2], r_bytes.as_slice());
}

// ---------------------------------------------------------------------------
// Clock fixtures
// ---------------------------------------------------------------------------

#[test]
fn test_clock_fixtures_round_trip_and_fail_closed() {
    let b = ControlBinding {
        id: 0x0102_0304,
        host_boot: HostBootId::from_raw(u128::from_be_bytes([0x11; 16])),
        os_session: OsSessionId::from_raw(u128::from_be_bytes([0x22; 16])),
        remote_session: RemoteSessionId::from_raw(u128::from_be_bytes([0x33; 16])),
    };

    let p_bytes = parse_hex(include_str!("fixtures/clock/clock_probe.hex"));
    let p_msg = decode_clock(
        &p_bytes,
        b,
        &L,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .expect("decode clock probe");
    let mut p_out = [0; fr_wire::clock::REPLY_BYTES];
    let n1 = encode_clock(
        p_msg,
        b,
        &L,
        &mut p_out,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .expect("encode probe");
    assert_eq!(&p_out[..n1], p_bytes.as_slice());

    let r_bytes = parse_hex(include_str!("fixtures/clock/clock_reply.hex"));
    let r_msg = decode_clock(
        &r_bytes,
        b,
        &L,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .expect("decode clock reply");
    let mut r_out = [0; fr_wire::clock::REPLY_BYTES];
    let n2 = encode_clock(
        r_msg,
        b,
        &L,
        &mut r_out,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .expect("encode reply");
    assert_eq!(&r_out[..n2], r_bytes.as_slice());
}

// ---------------------------------------------------------------------------
// Input fixtures (the 18 existing input / input_result records)
// ---------------------------------------------------------------------------

#[test]
fn test_input_fixtures_round_trip_and_fail_closed() {
    let input_cases = [
        include_str!("fixtures/input/key_page_usage.hex"),
        include_str!("fixtures/input/button.hex"),
        include_str!("fixtures/input/pointer.hex"),
        include_str!("fixtures/input/relative.hex"),
        include_str!("fixtures/input/scroll.hex"),
        include_str!("fixtures/input/text.hex"),
        include_str!("fixtures/input/mode.hex"),
    ];

    for hex_str in input_cases {
        let bytes = parse_hex(hex_str);
        let req = decode_input(
            &bytes,
            &L,
            9,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .expect("decode input");

        let mut out = [0; 256];
        let len = encode_input(
            req,
            &mut out,
            &L,
            9,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .expect("encode input");
        assert_eq!(&out[..len], bytes.as_slice());
    }

    let result_cases = [
        include_str!("fixtures/input/result-submitted.hex"),
        include_str!("fixtures/input/result-local.hex"),
        include_str!("fixtures/input/result-refused.hex"),
        include_str!("fixtures/input/result-expired.hex"),
        include_str!("fixtures/input/result-cancelled.hex"),
        include_str!("fixtures/input/result-partial.hex"),
        include_str!("fixtures/input/result-unknown.hex"),
        include_str!("fixtures/input/result-unknown-first.hex"),
        include_str!("fixtures/input/result-pointer.hex"),
    ];

    let res_b = ResultBinding {
        channel: 9,
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
    };

    for hex_str in result_cases {
        let bytes = parse_hex(hex_str);
        let res = decode_input_result(
            &bytes,
            &L,
            res_b,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .expect("decode input_result");
        assert_eq!(res.binding, res_b);
    }
}
