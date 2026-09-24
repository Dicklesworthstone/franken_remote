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
const H2V: InputDirection = InputDirection::HostToViewer;
const V2H: InputDirection = InputDirection::ViewerToHost;
const REL: InputDelivery = InputDelivery::Reliable;

macro_rules! fix {
    ($p:literal) => {
        include_str!(concat!("fixtures/", $p, ".hex"))
    };
}

fn parse_hex(hex_str: &str) -> Vec<u8> {
    let s = hex_str.trim();
    assert!(
        s.len().is_multiple_of(2),
        "hex string must have even length"
    );
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

fn check_negatives(bytes: &[u8], name: &str, mut decode_ok: impl FnMut(&[u8]) -> bool) {
    for cut in 0..bytes.len() {
        assert!(!decode_ok(&bytes[..cut]), "{name} cut at {cut} accepted");
    }
    let mut trailing = bytes.to_vec();
    trailing.push(0);
    assert!(!decode_ok(&trailing), "{name} trailing byte accepted");
}

fn assert_roundtrip_and_negatives<T: PartialEq + std::fmt::Debug, E: std::fmt::Debug>(
    bytes: &[u8],
    name: &str,
    mut decode: impl FnMut(&[u8]) -> Result<T, E>,
    mut encode: impl FnMut(&T, &mut [u8]) -> Result<usize, E>,
) {
    let msg = decode(bytes).unwrap_or_else(|e| panic!("{name} decode failed: {e:?}"));
    let mut buf = vec![0u8; bytes.len() + 256];
    let n = encode(&msg, &mut buf).unwrap_or_else(|e| panic!("{name} encode failed: {e:?}"));
    assert_eq!(&buf[..n], bytes, "{name} round-trip mismatch");
    check_negatives(bytes, name, |b| decode(b).is_ok());
}

// ---------------------------------------------------------------------------
// Negotiation fixtures
// ---------------------------------------------------------------------------

#[test]
fn test_negotiation_fixtures_round_trip_and_fail_closed() {
    for (hex_str, binding, name) in [
        (fix!("negotiation/client_hello"), 0, "client_hello"),
        (
            fix!("negotiation/host_capabilities"),
            0,
            "host_capabilities",
        ),
        (
            fix!("negotiation/selected_configuration"),
            0,
            "selected_configuration",
        ),
        (
            fix!("negotiation/approval_required"),
            0,
            "approval_required",
        ),
        (fix!("negotiation/session_opened"), 0, "session_opened"),
        (fix!("negotiation/binding_accepted"), 1, "binding_accepted"),
    ] {
        let bytes = parse_hex(hex_str);
        assert!(bytes.starts_with(b"FRD0"), "{name} bad magic");
        assert_roundtrip_and_negatives(
            &bytes,
            name,
            |b| decode_neg(b, 4096, binding),
            |m, out| encode_neg(m, 4096, out),
        );
    }
}

// ---------------------------------------------------------------------------
// Control fixtures
// ---------------------------------------------------------------------------

#[test]
fn test_control_fixtures_round_trip_and_fail_closed() {
    let parent = ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(1),
        os_session: OsSessionId::from_raw(2),
        remote_session: RemoteSessionId::from_raw(3),
    };
    let req_bytes = parse_hex(fix!("control/control_request"));
    assert_roundtrip_and_negatives(
        &req_bytes,
        "control_request",
        |b| decode_request(b, parent, &L, V2H, REL),
        |m, out| encode_request(*m, out, &L, V2H, REL),
    );
    assert_eq!(
        decode_request(&req_bytes, parent, &L, H2V, REL).unwrap_err(),
        WireError::WrongRole
    );

    let grant_bytes = parse_hex(fix!("control/lease_granted"));
    assert_roundtrip_and_negatives(
        &grant_bytes,
        "lease_granted",
        |b| decode_granted(b, parent, &L, H2V, REL),
        |m, out| encode_granted(*m, out, &L, H2V, REL),
    );
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
    for (hex_str, dir, name) in [
        (
            fix!("authority/challenge_observation"),
            H2V,
            "challenge_observation",
        ),
        (
            fix!("authority/challenge_control"),
            H2V,
            "challenge_control",
        ),
        (
            fix!("authority/response_observation"),
            V2H,
            "response_observation",
        ),
        (fix!("authority/response_control"), V2H, "response_control"),
    ] {
        let bytes = parse_hex(hex_str);
        assert_roundtrip_and_negatives(
            &bytes,
            name,
            |b| fr_wire::authority::decode(b, auth_b, &L, dir, REL),
            |m, out| fr_wire::authority::encode(*m, auth_b, &L, out, dir, REL),
        );
        let opp_dir = if dir == H2V { V2H } else { H2V };
        assert_eq!(
            fr_wire::authority::decode(&bytes, auth_b, &L, opp_dir, REL).unwrap_err(),
            WireError::WrongRole
        );
    }
}

// ---------------------------------------------------------------------------
// Input Ticket fixture
// ---------------------------------------------------------------------------

#[test]
fn test_input_ticket_fixture_round_trip_and_fail_closed() {
    let bytes = parse_hex(fix!("input_ticket/input_ticket"));
    assert_roundtrip_and_negatives(
        &bytes,
        "input_ticket",
        |b| decode_input_ticket(b, &L, 7, H2V, REL),
        |m, out| encode_input_ticket(*m, out, &L, 7, H2V, REL),
    );
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

    for (hex_str, channel, dir, name) in [
        (fix!("attachment/stream_binding"), 7, H2V, "stream_binding"),
        (
            fix!("attachment/binding_accepted"),
            7,
            V2H,
            "binding_accepted",
        ),
        (fix!("attachment/channel_ticket"), 7, H2V, "channel_ticket"),
        (fix!("attachment/channel_attach"), 8, V2H, "channel_attach"),
        (
            fix!("attachment/channel_attached"),
            8,
            H2V,
            "channel_attached",
        ),
    ] {
        let bytes = parse_hex(hex_str);
        assert_roundtrip_and_negatives(
            &bytes,
            name,
            |b| decode_attachment(b, parent, channel, &L, dir, REL),
            |m, out| encode_attachment(*m, parent, &L, out, dir, REL),
        );
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

    let cat_bytes = parse_hex(fix!("display/display_catalog"));
    assert_roundtrip_and_negatives(
        &cat_bytes,
        "display_catalog",
        |b| decode_display(b, parent, &L, H2V, REL),
        |m, out| encode_display(m, parent, &L, out, H2V, REL),
    );

    let sel_bytes = parse_hex(fix!("display/select_display"));
    assert_roundtrip_and_negatives(
        &sel_bytes,
        "select_display",
        |b| decode_display(b, parent, &L, V2H, REL),
        |m, out| encode_display(m, parent, &L, out, V2H, REL),
    );
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

    for (hex_str, dir, name) in [
        (
            fix!("decoder/decoder_configuration"),
            H2V,
            "decoder_configuration",
        ),
        (
            fix!("decoder/decoder_configured"),
            V2H,
            "decoder_configured",
        ),
        (
            fix!("decoder/first_frame_decoded"),
            V2H,
            "first_frame_decoded",
        ),
    ] {
        let bytes = parse_hex(hex_str);
        let msg = decode_decoder(&bytes, b, &L, dir, REL).expect("decode valid");
        let mut out = vec![0; 20000];
        let len = decode_encode(msg, b, &L, &mut out, dir, REL).expect("encode");
        assert_eq!(&out[..len], bytes.as_slice(), "{name} round-trip mismatch");
        check_negatives(&bytes, name, |data| {
            decode_decoder(data, b, &L, dir, REL).is_ok()
        });
    }
}

// ---------------------------------------------------------------------------
// Media fixtures
// ---------------------------------------------------------------------------

#[test]
fn test_media_fixtures_round_trip_and_fail_closed() {
    let med_limits = MediaLimits::new(L, 1_150, 16_384, 64).unwrap();

    let frag_bytes = parse_hex(fix!("media/access_unit_fragment"));
    let rec = Record::decode(&frag_bytes, &med_limits, 9, Channel::Video).expect("decode record");
    let frag = decode_fragment(rec, &med_limits).expect("decode fragment");
    let mut frag_out = [0; 128];
    let n1 = encode_fragment(frag, 9, &med_limits, &mut frag_out).expect("encode fragment");
    assert_eq!(&frag_out[..n1], frag_bytes.as_slice());

    let recovery_bytes = parse_hex(fix!("media/recovery_chunk"));
    let recovery_rec =
        Record::decode(&recovery_bytes, &med_limits, 9, Channel::Recovery).expect("decode record");
    let chunk = decode_recovery(recovery_rec, &med_limits).expect("decode recovery");
    let mut recovery_out = [0; 128];
    let n2 = encode_recovery(chunk, 9, &med_limits, &mut recovery_out).expect("encode recovery");
    assert_eq!(&recovery_out[..n2], recovery_bytes.as_slice());

    let repair_bytes = parse_hex(fix!("media/repair_request"));
    let repair_rec =
        Record::decode(&repair_bytes, &med_limits, 9, Channel::Control).expect("decode record");
    let rep = decode_repair(repair_rec, 7, &med_limits).expect("decode repair");
    let mut repair_out = [0; 128];
    let ranges: Vec<RepairRange> = rep.ranges().collect();
    let n3 = encode_repair(rep.frame, &ranges, 5, 9, &med_limits, &mut repair_out)
        .expect("encode repair");
    assert_eq!(&repair_out[..n3], repair_bytes.as_slice());

    let prog_bytes = parse_hex(fix!("media/media_progress"));
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
    let bytes = parse_hex(fix!("held_state/held_state"));
    assert_roundtrip_and_negatives(
        &bytes,
        "held_state",
        |data| decode_held(data, &L, 9, V2H, REL),
        |m, out| encode_held(*m, out, &L, 9, V2H, REL),
    );
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

    for (hex_str, name) in [
        (fix!("clipboard/clipboard_begin"), "clipboard_begin"),
        (fix!("clipboard/clipboard_chunk"), "clipboard_chunk"),
        (fix!("clipboard/clipboard_commit"), "clipboard_commit"),
        (fix!("clipboard/clipboard_cancel"), "clipboard_cancel"),
    ] {
        let bytes = parse_hex(hex_str);
        let msg = decode_clipboard(&bytes, ctx, &L).expect("decode valid");
        let mut out = vec![0; 4096];
        let len = encode_clipboard(msg, ctx, &L, &mut out).expect("encode");
        assert_eq!(&out[..len], bytes.as_slice(), "{name} round-trip mismatch");
        check_negatives(&bytes, name, |b| decode_clipboard(b, ctx, &L).is_ok());
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

    for (hex_str, role, name) in [
        (fix!("files/file_offer"), FileRole::Controller, "file_offer"),
        (fix!("files/file_accept"), FileRole::Host, "file_accept"),
        (fix!("files/file_chunk"), FileRole::Controller, "file_chunk"),
        (fix!("files/file_complete"), FileRole::Host, "file_complete"),
        (
            fix!("files/file_cancel"),
            FileRole::Controller,
            "file_cancel",
        ),
    ] {
        let bytes = parse_hex(hex_str);
        let msg = decode_files(&bytes, ctx(role), file_limits).expect("decode valid");
        let mut out = [0; 4096];
        let len = encode_files(msg, ctx(role), file_limits, &mut out).expect("encode");
        assert_eq!(&out[..len], bytes.as_slice(), "{name} round-trip mismatch");
        check_negatives(&bytes, name, |b| {
            decode_files(b, ctx(role), file_limits).is_ok()
        });
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
        (fix!("presented/presented_state_sample"), "presented_sample"),
        (
            fix!("presented/presented_state_unavailable"),
            "presented_unavailable",
        ),
    ] {
        let bytes = parse_hex(hex_str);
        assert_roundtrip_and_negatives(
            &bytes,
            name,
            |data| decode_presented(data, b, &L, V2H, REL),
            |m, out| encode_presented(*m, b, &L, out, V2H, REL),
        );
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

    for (hex_str, dir, name) in [
        (fix!("receiver_metrics/metrics_query"), H2V, "metrics_query"),
        (fix!("receiver_metrics/metrics_reply"), V2H, "metrics_reply"),
    ] {
        let bytes = parse_hex(hex_str);
        assert_roundtrip_and_negatives(
            &bytes,
            name,
            |data| decode_metrics(data, b, &L, dir, REL),
            |m, out| encode_metrics(*m, b, &L, out, dir, REL),
        );
    }
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

    for (hex_str, dir, name) in [
        (fix!("clock/clock_probe"), V2H, "clock_probe"),
        (fix!("clock/clock_reply"), H2V, "clock_reply"),
    ] {
        let bytes = parse_hex(hex_str);
        assert_roundtrip_and_negatives(
            &bytes,
            name,
            |data| decode_clock(data, b, &L, dir, REL),
            |m, out| encode_clock(*m, b, &L, out, dir, REL),
        );
    }
}

// ---------------------------------------------------------------------------
// Input fixtures (the 18 existing input / input_result records)
// ---------------------------------------------------------------------------

#[test]
fn test_input_fixtures_round_trip_and_fail_closed() {
    for hex_str in [
        fix!("input/key_page_usage"),
        fix!("input/button"),
        fix!("input/pointer"),
        fix!("input/relative"),
        fix!("input/scroll"),
        fix!("input/text"),
        fix!("input/mode"),
    ] {
        let bytes = parse_hex(hex_str);
        let req = decode_input(&bytes, &L, 9, V2H, REL).expect("decode input");
        let mut out = [0; 256];
        let len = encode_input(req, &mut out, &L, 9, V2H, REL).expect("encode input");
        assert_eq!(&out[..len], bytes.as_slice());
        check_negatives(&bytes, "input", |b| {
            decode_input(b, &L, 9, V2H, REL).is_ok()
        });
    }

    let res_b = ResultBinding {
        channel: 9,
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
    };

    for hex_str in [
        fix!("input/result-submitted"),
        fix!("input/result-local"),
        fix!("input/result-refused"),
        fix!("input/result-expired"),
        fix!("input/result-cancelled"),
        fix!("input/result-partial"),
        fix!("input/result-unknown"),
        fix!("input/result-unknown-first"),
        fix!("input/result-pointer"),
    ] {
        let bytes = parse_hex(hex_str);
        let res = decode_input_result(&bytes, &L, res_b, H2V, REL).expect("decode input_result");
        assert_eq!(res.binding, res_b);
        check_negatives(&bytes, "input_result", |b| {
            decode_input_result(b, &L, res_b, H2V, REL).is_ok()
        });
    }
}
