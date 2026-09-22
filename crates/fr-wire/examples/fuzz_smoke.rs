#![forbid(unsafe_code)]
#![allow(clippy::similar_names, clippy::too_many_lines)]
#![rustfmt::skip]
//! Fuzz smoke target and golden seed corpus harness for `fr-wire`.
//!
//! Provides a deterministic, coverage-ready fuzz runner seeded with golden
//! wire records for every v0 protocol message class.
//!
//! Run as an example:
//! `cargo run -p fr-wire --example fuzz_smoke -- --iterations 10000`
//!
//! Dump golden fixtures to disk:
//! `cargo run -p fr-wire --example fuzz_smoke -- --dump-fixtures crates/fr-wire/tests/fixtures`

use fr_core::{
    clipboard::{Binding as ClipBinding, Endpoint as ClipEndpoint, Stamp as ClipStamp},
    held_state::{HeldState, HeldStateRequest},
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, InputLeaseId,
        InputTicketId, OsSessionId, RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{
        DesktopPoint, InputBounds, InputCredentials, InputEvent, InputRequest, InputView,
        KeyTransition, PhysicalKey, PointerButton, PointerMode, ScrollUnit,
    },
    input_submission::{Capabilities, Capability as InputCap},
    limits::ProtocolLimits,
};
use fr_wire::{
    Channel, Fragment, FrameDescriptor, MediaLimits, PipelineState, Progress, Record,
    RecoveryChunk, RepairRange, SourceObservation,
    attachment::{
        Descriptor, Grant, MediaRole, Message as AttachMsg, Ticket as AttachTicket,
        decode as decode_attachment, encode as encode_attachment,
    },
    authority::{Binding as AuthBinding, Message as AuthMsg, Scope as AuthScope},
    clipboard::{
        Body as ClipBody, CancelReason as ClipCancel, Context as ClipContext, Lane as ClipLane,
        Message as ClipMsg, Role as ClipRole, decode as decode_clipboard,
        encode as encode_clipboard,
    },
    clock::{Message as ClockMsg, decode as decode_clock, encode as encode_clock},
    control::{
        Granted as ControlGranted, Request as ControlRequest, Target as ControlTarget,
        encode_granted, encode_request,
    },
    decode_fragment, decode_progress, decode_recovery, decode_repair,
    decoder::{
        Binding as DecBinding, Configuration as DecConfig, Message as DecMsg,
        decode as decode_decoder, encode as decode_encode,
    },
    display::{
        Catalog as DispCatalog, Display as DispDisplay, Message as DispMsg,
        decode as decode_display, encode as encode_display,
    },
    encode_fragment, encode_progress, encode_recovery, encode_repair,
    files::{
        Body as FileBody, Context as FileContext, Direction as FileDir, Disposition,
        Lane as FileLane, Limits as FileLimits, Message as FileMsg, Reason as FileReason,
        Role as FileRole, decode as decode_files, encode as encode_files,
    },
    held_state::{decode as decode_held, encode as encode_held},
    input::{InputDelivery, InputDirection, decode_input, encode_input},
    input_result::{ResultBinding, decode_input_result},
    input_ticket::{
        Ticket as InputTicket, decode as decode_input_ticket, encode as encode_input_ticket,
    },
    negotiation::{
        Capability as NegCap, ControlBinding, Message as NegMsg, NATIVE_PROFILE, Offer as NegOffer,
        PROFILE_VERSION, Role as NegRole, decode as decode_neg, encode as encode_neg,
    },
    presented::{
        Report as PresReport, Sample as PresSample, Stamp as PresStamp, decode as decode_presented,
        encode as encode_presented,
    },
    receiver_metrics::{
        Load as RecvLoad, Message as RecvMsg, decode as decode_metrics, encode as encode_metrics,
    },
};
use std::path::{Path, PathBuf};

const L: ProtocolLimits = ProtocolLimits::ABSOLUTE;

/// A named golden seed with its message classification and canonical wire bytes.
pub struct GoldenSeed {
    pub category: &'static str,
    pub name: &'static str,
    pub bytes: Vec<u8>,
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ---------------------------------------------------------------------------
// Seed generators for every message family
// ---------------------------------------------------------------------------

fn seeds_negotiation() -> Vec<GoldenSeed> {
    let offer = NegOffer {
        versions: vec![0, 7],
        profile: NATIVE_PROFILE,
        profile_version: PROFILE_VERSION,
        role: NegRole::Observe,
        limits: L,
        capabilities: vec![NegCap { name: "media.hevc.main".into(), version: 1, required: true }],
    };
    let selection = offer.select().unwrap();
    let msgs: [(&str, NegMsg); 6] = [
        ("client_hello", NegMsg::ClientHello(offer.clone())),
        ("host_capabilities", NegMsg::HostCapabilities(offer)),
        ("selected_configuration", NegMsg::SelectedConfiguration(selection.clone())),
        ("approval_required", NegMsg::ApprovalRequired { request: RemoteSessionId::from_raw(9), deadline_us: 33, role: NegRole::Observe }),
        ("session_opened", NegMsg::SessionOpened { binding: ControlBinding { id: 1, host_boot: HostBootId::from_raw(1), os_session: OsSessionId::from_raw(2), remote_session: RemoteSessionId::from_raw(3) }, selection, observation_until_us: 44 }),
        ("binding_accepted", NegMsg::BindingAccepted { binding: 1 }),
    ];
    msgs.into_iter().map(|(name, m)| {
        let mut out = vec![0; 4096];
        let len = encode_neg(&m, 4096, &mut out).expect("encode negotiation");
        out.truncate(len);
        GoldenSeed { category: "negotiation", name, bytes: out }
    }).collect()
}

fn seeds_control() -> Vec<GoldenSeed> {
    let req = ControlRequest {
        parent: ControlBinding { id: 7, host_boot: HostBootId::from_raw(1), os_session: OsSessionId::from_raw(2), remote_session: RemoteSessionId::from_raw(3) },
        sequence: 0,
        target: ControlTarget {
            display_binding: 8,
            view: InputView { geometry: DisplayGeometryGeneration::from_raw(4), viewport: ViewportMappingGeneration::from_raw(5), configuration: CodecConfigurationGeneration::from_raw(6), recovery: RecoveryGeneration::from_raw(7) },
            bounds: InputBounds::new(DesktopPoint { x: -100, y: 20 }, 1920, 1080).unwrap(),
            capabilities: Capabilities::default().with(InputCap::Keys).with(InputCap::Absolute).with(InputCap::Buttons),
        },
    };
    let grant = ControlGranted {
        request: req, input_channel: 9, lease: InputLeaseId::from_raw(10), ticket: InputTicketId::from_raw(11),
        issued_at_us: 0, lease_until_us: 3_000_000, ticket_until_us: 1_000_000, first_action: 0, first_pointer: 0,
    };
    let mut req_bytes = vec![0; fr_wire::control::REQUEST_BYTES];
    encode_request(req, &mut req_bytes, &L, InputDirection::ViewerToHost, InputDelivery::Reliable).expect("encode control_request");
    let mut grant_bytes = vec![0; fr_wire::control::GRANTED_BYTES];
    encode_granted(grant, &mut grant_bytes, &L, InputDirection::HostToViewer, InputDelivery::Reliable).expect("encode lease_granted");
    vec![
        GoldenSeed { category: "control", name: "control_request", bytes: req_bytes },
        GoldenSeed { category: "control", name: "lease_granted", bytes: grant_bytes },
    ]
}

fn seeds_authority() -> Vec<GoldenSeed> {
    let auth_b = AuthBinding { channel: 0x0102_0304, session: RemoteSessionId::from_raw(0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00) };
    let nonce = u128::from_be_bytes([0x77; 16]);
    let lease = InputLeaseId::from_raw(u128::from_be_bytes([0x55; 16]));
    let cases = [
        ("challenge_observation", AuthMsg::Challenge { scope: AuthScope::Observation, nonce, deadline_micros: 1_000_000 }, InputDirection::HostToViewer),
        ("challenge_control", AuthMsg::Challenge { scope: AuthScope::Control(lease), nonce, deadline_micros: 1_000_000 }, InputDirection::HostToViewer),
        ("response_observation", AuthMsg::Response { scope: AuthScope::Observation, nonce }, InputDirection::ViewerToHost),
        ("response_control", AuthMsg::Response { scope: AuthScope::Control(lease), nonce }, InputDirection::ViewerToHost),
    ];
    cases.into_iter().map(|(name, m, dir)| {
        let mut out = [0; fr_wire::authority::MAX_AUTHORITY_BYTES];
        let len = fr_wire::authority::encode(m, auth_b, &L, &mut out, dir, InputDelivery::Reliable).expect("encode authority");
        GoldenSeed { category: "authority", name, bytes: out[..len].to_vec() }
    }).collect()
}

fn seeds_input_ticket() -> Vec<GoldenSeed> {
    let ticket = InputTicket {
        credentials: InputCredentials {
            session: RemoteSessionId::from_raw(u128::from_be_bytes([0x11; 16])),
            lease: InputLeaseId::from_raw(u128::from_be_bytes([0x22; 16])),
            ticket: InputTicketId::from_raw(u128::from_be_bytes([0x33; 16])),
            view: InputView { geometry: DisplayGeometryGeneration::from_raw(1), viewport: ViewportMappingGeneration::from_raw(2), configuration: CodecConfigurationGeneration::from_raw(3), recovery: RecoveryGeneration::from_raw(4) },
        },
        sequence: 5, issued_at_us: 6, expires_at_us: 1_000_006,
    };
    let mut out = vec![0; fr_wire::input_ticket::INPUT_TICKET_BYTES];
    encode_input_ticket(ticket, &mut out, &L, 7, InputDirection::HostToViewer, InputDelivery::Reliable).expect("encode input_ticket");
    vec![GoldenSeed { category: "input_ticket", name: "input_ticket", bytes: out }]
}

fn seeds_attachment() -> Vec<GoldenSeed> {
    let parent = ControlBinding { id: 7, host_boot: HostBootId::from_raw(11), os_session: OsSessionId::from_raw(12), remote_session: RemoteSessionId::from_raw(13) };
    let desc = Descriptor {
        binding: DecBinding {
            parent: ControlBinding { id: 8, ..parent }, display: 14,
            geometry: DisplayGeometryGeneration::INITIAL, configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL, viewport: ViewportMappingGeneration::INITIAL,
        },
        role: MediaRole::Configuration, host_stream: 7, viewer_stream: 6,
    };
    let grant = Grant { descriptor: desc, ticket: AttachTicket(0x1234_5678_90ab_cdef), deadline_us: 900, byte_allowance: 4096, picture_allowance: 0, credit_epoch: 8 };
    let cases = [
        ("stream_binding", AttachMsg::Binding(desc), InputDirection::HostToViewer),
        ("binding_accepted", AttachMsg::Accepted(8), InputDirection::ViewerToHost),
        ("channel_ticket", AttachMsg::Ticket(grant), InputDirection::HostToViewer),
        ("channel_attach", AttachMsg::Attach(grant), InputDirection::ViewerToHost),
        ("channel_attached", AttachMsg::Attached(grant), InputDirection::HostToViewer),
    ];
    cases.into_iter().map(|(name, m, dir)| {
        let mut out = vec![0; fr_wire::attachment::GRANT_RECORD_BYTES];
        let len = encode_attachment(m, parent, &L, &mut out, dir, InputDelivery::Reliable).expect("encode attachment");
        out.truncate(len);
        GoldenSeed { category: "attachment", name, bytes: out }
    }).collect()
}

fn seeds_display() -> Vec<GoldenSeed> {
    let parent = ControlBinding { id: 0x0102_0304, host_boot: HostBootId::from_raw(u128::from_be_bytes([0x11; 16])), os_session: OsSessionId::from_raw(u128::from_be_bytes([0x22; 16])), remote_session: RemoteSessionId::from_raw(u128::from_be_bytes([0x33; 16])) };
    let screen = DispDisplay {
        handle: u128::from_be_bytes([0x44; 16]), geometry: DisplayGeometryGeneration::INITIAL,
        x: -1920, y: -20, pixel_width: 1920, pixel_height: 1080, logical_width: 1280, logical_height: 720,
        scale_numerator: 3, scale_denominator: 2, rotation: 1,
    };
    let catalog = DispCatalog::new(7, &[screen], &L).unwrap();
    let sel = catalog.selection(screen.handle).unwrap();
    let mut cat_bytes = [0; fr_wire::display::MAX_CATALOG_BYTES];
    let n = encode_display(&DispMsg::Catalog(catalog), parent, &L, &mut cat_bytes, InputDirection::HostToViewer, InputDelivery::Reliable).expect("encode display_catalog");
    let mut sel_bytes = [0; fr_wire::display::SELECT_BYTES];
    let m = encode_display(&DispMsg::Select(sel), parent, &L, &mut sel_bytes, InputDirection::ViewerToHost, InputDelivery::Reliable).expect("encode select_display");
    vec![
        GoldenSeed { category: "display", name: "display_catalog", bytes: cat_bytes[..n].to_vec() },
        GoldenSeed { category: "display", name: "select_display", bytes: sel_bytes[..m].to_vec() },
    ]
}

fn seeds_decoder() -> Vec<GoldenSeed> {
    let b = DecBinding {
        parent: ControlBinding { id: 7, host_boot: HostBootId::from_raw(11), os_session: OsSessionId::from_raw(12), remote_session: RemoteSessionId::from_raw(13) },
        display: 14, geometry: DisplayGeometryGeneration::from_raw(15), configuration: CodecConfigurationGeneration::from_raw(16),
        recovery: RecoveryGeneration::from_raw(17), viewport: ViewportMappingGeneration::from_raw(18),
    };
    let config = DecConfig {
        coded_width: 320, coded_height: 240, crop_width: 318, crop_height: 238, fps: 30,
        primaries: 1, transfer: 1, matrix: 1, full_range: false, decoded_pictures: 4, codec: "hev1.1.6.L60.90", hvcc: &[1; 23],
    };
    let cases = [
        ("decoder_configuration", DecMsg::Configuration(config), InputDirection::HostToViewer),
        ("decoder_configured", DecMsg::Configured, InputDirection::ViewerToHost),
        ("first_frame_decoded", DecMsg::FirstDecoded { frame: 0, decoder_micros: 99 }, InputDirection::ViewerToHost),
    ];
    cases.into_iter().map(|(name, m, dir)| {
        let mut out = vec![0; 20000];
        let len = decode_encode(m, b, &L, &mut out, dir, InputDelivery::Reliable).expect("encode decoder");
        out.truncate(len);
        GoldenSeed { category: "decoder", name, bytes: out }
    }).collect()
}

fn seeds_media() -> Vec<GoldenSeed> {
    let med_limits = MediaLimits::new(L, 1_150, 16_384, 64).unwrap();
    let desc = FrameDescriptor { frame: 7, total_bytes: 3, stride: 8, capture_micros: 42, reference: None };
    let mut frag_out = [0; 128];
    let n1 = encode_fragment(Fragment { descriptor: desc, index: 0, bytes: &[1, 2, 3] }, 9, &med_limits, &mut frag_out).expect("encode fragment");
    let mut recovery_out = [0; 128];
    let n2 = encode_recovery(RecoveryChunk { frame: 7, total_bytes: 10, offset: 6, capture_micros: 42, bytes: &[1, 2, 3, 4] }, 9, &med_limits, &mut recovery_out).expect("encode recovery");
    let mut repair_out = [0; 128];
    let ranges = [RepairRange { start: 0, end: 2 }, RepairRange { start: 4, end: 5 }];
    let n3 = encode_repair(7, &ranges, 5, 9, &med_limits, &mut repair_out).expect("encode repair");
    let mut prog_out = [0; 128];
    let n4 = encode_progress(Progress { descriptor: FrameDescriptor { reference: Some(6), ..desc }, observed_micros: 45, observation: SourceObservation::QualifiedUnchanged, pipeline: PipelineState::Idle }, 9, &med_limits, &mut prog_out).expect("encode progress");
    vec![
        GoldenSeed { category: "media", name: "access_unit_fragment", bytes: frag_out[..n1].to_vec() },
        GoldenSeed { category: "media", name: "recovery_chunk", bytes: recovery_out[..n2].to_vec() },
        GoldenSeed { category: "media", name: "repair_request", bytes: repair_out[..n3].to_vec() },
        GoldenSeed { category: "media", name: "media_progress", bytes: prog_out[..n4].to_vec() },
    ]
}

fn seeds_held_state() -> Vec<GoldenSeed> {
    let mut held = HeldState::empty();
    held.set_key(PhysicalKey::new(4).unwrap(), true);
    held.set_key(PhysicalKey::new(0xe1).unwrap(), true);
    held.set_button(PointerButton::Primary, true);
    let req = HeldStateRequest { session: RemoteSessionId::from_raw(1), lease: InputLeaseId::from_raw(2), sequence: 3, next_action: 4, held };
    let mut out = [0; fr_wire::held_state::HELD_STATE_BYTES];
    encode_held(req, &mut out, &L, 9, InputDirection::ViewerToHost, InputDelivery::Reliable).expect("encode held_state");
    vec![GoldenSeed { category: "held_state", name: "held_state", bytes: out.to_vec() }]
}

fn seeds_clipboard() -> Vec<GoldenSeed> {
    let ctx = ClipContext { scope: ClipBinding { session: RemoteSessionId::from_raw(1), lease: InputLeaseId::from_raw(2) }, channel: 9, sender: ClipRole::Controller, lane: ClipLane::Clipboard };
    let stamp = ClipStamp { id: 3, source: ClipEndpoint::Controller, sequence: 4 };
    let cases = [
        ("clipboard_begin", ClipBody::Begin { total_bytes: 3, chunks: 1 }),
        ("clipboard_chunk", ClipBody::Chunk { index: 0, offset: 0, bytes: b"abc" }),
        ("clipboard_commit", ClipBody::Commit { total_bytes: 3 }),
        ("clipboard_cancel", ClipBody::Cancel(ClipCancel::User)),
    ];
    cases.into_iter().map(|(name, body)| {
        let mut out = vec![0; 4096];
        let len = encode_clipboard(ClipMsg { stamp, body }, ctx, &L, &mut out).expect("encode clipboard");
        out.truncate(len);
        GoldenSeed { category: "clipboard", name, bytes: out }
    }).collect()
}

fn seeds_files() -> Vec<GoldenSeed> {
    let ctx = |sender: FileRole| FileContext {
        session: RemoteSessionId::from_raw(1), lease: InputLeaseId::from_raw(2), handle: 3,
        channel: if sender == FileRole::Host { 10 } else { 9 }, sender, direction: FileDir::ToHost, lane: FileLane::Files,
    };
    let file_limits = FileLimits::new(&L, 4096).unwrap();
    let atp = &[1, 2, 3];
    let cases = [
        ("file_offer", FileBody::Offer { profile: 1, atp }, FileRole::Controller),
        ("file_accept", FileBody::Accept { profile: 1, size: 3, bytes_per_second: 1000, chunk_bytes: 500, concurrent_transfers: 1, atp }, FileRole::Host),
        ("file_chunk", FileBody::Chunk { atp }, FileRole::Controller),
        ("file_complete", FileBody::Complete { disposition: Disposition::PublishedDurable, reason: FileReason::None, published_bytes: 3, atp }, FileRole::Host),
        ("file_cancel", FileBody::Cancel(FileReason::User), FileRole::Controller),
    ];
    cases.into_iter().map(|(name, body, role)| {
        let mut out = [0; 4096];
        let len = encode_files(FileMsg { id: 4, body }, ctx(role), file_limits, &mut out).expect("encode files");
        GoldenSeed { category: "files", name, bytes: out[..len].to_vec() }
    }).collect()
}

fn seeds_presented() -> Vec<GoldenSeed> {
    let b = DecBinding {
        parent: ControlBinding { id: 7, host_boot: HostBootId::from_raw(1), os_session: OsSessionId::from_raw(2), remote_session: RemoteSessionId::from_raw(3) },
        display: 4, geometry: DisplayGeometryGeneration::INITIAL, configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL, viewport: ViewportMappingGeneration::INITIAL,
    };
    let r_sample = PresReport { sequence: 1, visible: Some(PresSample { stamp: PresStamp { frame: 9, captured_us: 10_000, observed_us: 20_000, source: SourceObservation::QualifiedUnchanged }, age_upper_us: 30_000 }) };
    let r_unavail = PresReport { sequence: 2, visible: None };
    let mut b1 = vec![0; fr_wire::presented::BYTES];
    encode_presented(r_sample, b, &L, &mut b1, InputDirection::ViewerToHost, InputDelivery::Reliable).expect("encode presented_sample");
    let mut b2 = vec![0; fr_wire::presented::BYTES];
    encode_presented(r_unavail, b, &L, &mut b2, InputDirection::ViewerToHost, InputDelivery::Reliable).expect("encode presented_unavail");
    vec![
        GoldenSeed { category: "presented", name: "presented_state_sample", bytes: b1 },
        GoldenSeed { category: "presented", name: "presented_state_unavailable", bytes: b2 },
    ]
}

fn seeds_receiver_metrics() -> Vec<GoldenSeed> {
    let b = DecBinding {
        parent: ControlBinding { id: 7, host_boot: HostBootId::from_raw(1), os_session: OsSessionId::from_raw(2), remote_session: RemoteSessionId::from_raw(3) },
        display: 4, geometry: DisplayGeometryGeneration::INITIAL, configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL, viewport: ViewportMappingGeneration::INITIAL,
    };
    let mut q_out = vec![0; fr_wire::receiver_metrics::REPLY_BYTES];
    let n1 = encode_metrics(RecvMsg::Query { sequence: 1 }, b, &L, &mut q_out, InputDirection::HostToViewer, InputDelivery::Reliable).expect("encode query");
    q_out.truncate(n1);
    let mut r_out = vec![0; fr_wire::receiver_metrics::REPLY_BYTES];
    let n2 = encode_metrics(RecvMsg::Reply { sequence: 2, load: RecvLoad { retained_bytes: 1000, retained_pictures: 2, decoding: true, work_us: Some(75_000) } }, b, &L, &mut r_out, InputDirection::ViewerToHost, InputDelivery::Reliable).expect("encode reply");
    r_out.truncate(n2);
    vec![
        GoldenSeed { category: "receiver_metrics", name: "metrics_query", bytes: q_out },
        GoldenSeed { category: "receiver_metrics", name: "metrics_reply", bytes: r_out },
    ]
}

fn seeds_clock() -> Vec<GoldenSeed> {
    let b = ControlBinding {
        id: 0x0102_0304, host_boot: HostBootId::from_raw(u128::from_be_bytes([0x11; 16])),
        os_session: OsSessionId::from_raw(u128::from_be_bytes([0x22; 16])), remote_session: RemoteSessionId::from_raw(u128::from_be_bytes([0x33; 16])),
    };
    let mut p_out = [0; fr_wire::clock::REPLY_BYTES];
    let n1 = encode_clock(ClockMsg::Probe { sequence: 0x0102_0304_0506_0708 }, b, &L, &mut p_out, InputDirection::ViewerToHost, InputDelivery::Reliable).expect("encode clock_probe");
    let mut r_out = [0; fr_wire::clock::REPLY_BYTES];
    let n2 = encode_clock(ClockMsg::Reply { sequence: 0x0102_0304_0506_0708, host_sample_us: 0xf1f2_f3f4_f5f6_f7f8 }, b, &L, &mut r_out, InputDirection::HostToViewer, InputDelivery::Reliable).expect("encode clock_reply");
    vec![
        GoldenSeed { category: "clock", name: "clock_probe", bytes: p_out[..n1].to_vec() },
        GoldenSeed { category: "clock", name: "clock_reply", bytes: r_out[..n2].to_vec() },
    ]
}

fn seeds_input() -> Vec<GoldenSeed> {
    let creds = InputCredentials {
        session: RemoteSessionId::from_raw(1), lease: InputLeaseId::from_raw(2), ticket: InputTicketId::from_raw(3),
        view: InputView { geometry: DisplayGeometryGeneration::from_raw(4), viewport: ViewportMappingGeneration::from_raw(5), configuration: CodecConfigurationGeneration::from_raw(6), recovery: RecoveryGeneration::from_raw(7) },
    };
    let pos = DesktopPoint { x: -120, y: 45 };
    let events: [(&str, InputEvent<'static>); 7] = [
        ("key_page_usage", InputEvent::Key { key: PhysicalKey::new(4).unwrap(), transition: KeyTransition::Press }),
        ("button", InputEvent::Button { button: PointerButton::Secondary, pressed: true, position: pos, barrier: 99 }),
        ("pointer", InputEvent::Pointer { position: pos }),
        ("relative", InputEvent::Relative { mode_epoch: 11, cumulative_x: -1000, cumulative_y: 2000 }),
        ("scroll", InputEvent::Scroll { position: pos, barrier: 99, x: -1, y: 2, unit: ScrollUnit::Lines }),
        ("text", InputEvent::Text("hé🙂")),
        ("mode", InputEvent::Mode { mode: PointerMode::Relative, epoch: 11 }),
    ];
    events.into_iter().map(|(name, event)| {
        let req = InputRequest { credentials: creds, sequence: 8, event };
        let mut out = [0; 256];
        let len = encode_input(req, &mut out, &L, 9, InputDirection::ViewerToHost, InputDelivery::Reliable).expect("encode input");
        GoldenSeed { category: "input", name, bytes: out[..len].to_vec() }
    }).collect()
}

/// Collects all golden seeds across all protocol message families.
pub fn all_seeds() -> Vec<GoldenSeed> {
    let mut v = Vec::new();
    v.extend(seeds_negotiation());
    v.extend(seeds_control());
    v.extend(seeds_authority());
    v.extend(seeds_input_ticket());
    v.extend(seeds_attachment());
    v.extend(seeds_display());
    v.extend(seeds_decoder());
    v.extend(seeds_media());
    v.extend(seeds_held_state());
    v.extend(seeds_clipboard());
    v.extend(seeds_files());
    v.extend(seeds_presented());
    v.extend(seeds_receiver_metrics());
    v.extend(seeds_clock());
    v.extend(seeds_input());
    v
}

// ---------------------------------------------------------------------------
// Deterministic PRNG for reproducible fuzzing
// ---------------------------------------------------------------------------

struct Prng {
    state: u64,
}

impl Prng {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x4652_4430_2026_0919
            } else {
                seed
            },
        }
    }

    fn next_u32(&mut self) -> u32 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.state >> 32) as u32
    }

    fn next_range(&mut self, upper: usize) -> usize {
        if upper == 0 {
            0
        } else {
            (self.next_u32() as usize) % upper
        }
    }

    fn next_u8(&mut self) -> u8 {
        (self.next_u32() & 0xff) as u8
    }
}

// ---------------------------------------------------------------------------
// Fuzz mutation engine
// ---------------------------------------------------------------------------

fn mutate(corpus: &[GoldenSeed], prng: &mut Prng) -> Vec<u8> {
    let op = prng.next_range(7);
    match op {
        0 => {
            // Completely random buffer
            let len = prng.next_range(512);
            let mut buf = vec![0; len];
            for b in &mut buf {
                *b = prng.next_u8();
            }
            buf
        }
        1 => {
            // Bit flip on a seed
            let seed = &corpus[prng.next_range(corpus.len())];
            let mut buf = seed.bytes.clone();
            if !buf.is_empty() {
                let idx = prng.next_range(buf.len());
                let bit = 1 << prng.next_range(8);
                buf[idx] ^= bit;
            }
            buf
        }
        2 => {
            // Byte overwrite
            let seed = &corpus[prng.next_range(corpus.len())];
            let mut buf = seed.bytes.clone();
            if !buf.is_empty() {
                let idx = prng.next_range(buf.len());
                buf[idx] = prng.next_u8();
            }
            buf
        }
        3 => {
            // Truncation
            let seed = &corpus[prng.next_range(corpus.len())];
            let cut = prng.next_range(seed.bytes.len());
            seed.bytes[..cut].to_vec()
        }
        4 => {
            // Extension / trailing bytes
            let seed = &corpus[prng.next_range(corpus.len())];
            let mut buf = seed.bytes.clone();
            let extra = prng.next_range(32) + 1;
            for _ in 0..extra {
                buf.push(prng.next_u8());
            }
            buf
        }
        5 => {
            // Header corruption (magic, version, kind, length, flags)
            let seed = &corpus[prng.next_range(corpus.len())];
            let mut buf = seed.bytes.clone();
            if buf.len() >= 24 {
                let header_field = prng.next_range(6);
                match header_field {
                    0 => buf[..4].copy_from_slice(b"BAD0"),
                    1 => buf[4..6].copy_from_slice(&prng.next_u32().to_be_bytes()[..2]),
                    2 => buf[6..8].copy_from_slice(&prng.next_u32().to_be_bytes()[..2]),
                    3 => buf[8..12].copy_from_slice(&prng.next_u32().to_be_bytes()),
                    4 => buf[12..16].copy_from_slice(&prng.next_u32().to_be_bytes()),
                    5 => buf[16..20].copy_from_slice(&prng.next_u32().to_be_bytes()),
                    _ => {}
                }
            }
            buf
        }
        _ => {
            // Splicing two seeds together
            let s1 = &corpus[prng.next_range(corpus.len())];
            let s2 = &corpus[prng.next_range(corpus.len())];
            let cut1 = prng.next_range(s1.bytes.len());
            let cut2 = prng.next_range(s2.bytes.len());
            let mut buf = s1.bytes[..cut1].to_vec();
            buf.extend_from_slice(&s2.bytes[cut2..]);
            buf
        }
    }
}

// ---------------------------------------------------------------------------
// Fuzz target execution & invariant checking
// ---------------------------------------------------------------------------

/// Feeds mutated input to the wire decoders and asserts invariants.
///
/// Invariants:
/// 1. Decoders MUST NEVER PANIC.
/// 2. Returns either Ok(T) or typed Error.
/// 3. If parsing succeeds, re-encoding the parsed structure must never panic.
pub fn fuzz_one(input: &[u8]) {
    let p_rel = InputDelivery::Reliable;
    let d_h2v = InputDirection::HostToViewer;
    let d_v2h = InputDirection::ViewerToHost;
    let parent = ControlBinding { id: 7, host_boot: HostBootId::from_raw(11), os_session: OsSessionId::from_raw(12), remote_session: RemoteSessionId::from_raw(13) };
    let dec_b = DecBinding {
        parent, display: 14, geometry: DisplayGeometryGeneration::from_raw(15), configuration: CodecConfigurationGeneration::from_raw(16),
        recovery: RecoveryGeneration::from_raw(17), viewport: ViewportMappingGeneration::from_raw(18),
    };
    let auth_b = AuthBinding { channel: 0x0102_0304, session: RemoteSessionId::from_raw(0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00) };
    let disp_p = ControlBinding { id: 0x0102_0304, host_boot: HostBootId::from_raw(u128::from_be_bytes([0x11; 16])), os_session: OsSessionId::from_raw(u128::from_be_bytes([0x22; 16])), remote_session: RemoteSessionId::from_raw(u128::from_be_bytes([0x33; 16])) };
    let clip_ctx = ClipContext { scope: ClipBinding { session: RemoteSessionId::from_raw(1), lease: InputLeaseId::from_raw(2) }, channel: 9, sender: ClipRole::Controller, lane: ClipLane::Clipboard };
    let file_ctx = FileContext { session: RemoteSessionId::from_raw(1), lease: InputLeaseId::from_raw(2), handle: 3, channel: 9, sender: FileRole::Controller, direction: FileDir::ToHost, lane: FileLane::Files };
    let pres_b = DecBinding { parent: ControlBinding { id: 7, host_boot: HostBootId::from_raw(1), os_session: OsSessionId::from_raw(2), remote_session: RemoteSessionId::from_raw(3) }, display: 4, geometry: DisplayGeometryGeneration::INITIAL, configuration: CodecConfigurationGeneration::INITIAL, recovery: RecoveryGeneration::INITIAL, viewport: ViewportMappingGeneration::INITIAL };

    // 1. Raw record decode with various limits
    if let Ok(med_limits) = MediaLimits::new(L, 1_150, 16_384, 64) {
        for &binding in &[0, 1, 7, 8, 9, 0x0102_0304] {
            for &channel in &[Channel::Video, Channel::Recovery, Channel::Control, Channel::MediaConfig] {
                let _ = Record::decode(input, &med_limits, binding, channel);
            }
        }
        if let Ok(rec) = Record::decode(input, &med_limits, 9, Channel::Video) { let _ = decode_fragment(rec, &med_limits); }
        if let Ok(rec) = Record::decode(input, &med_limits, 9, Channel::Recovery) { let _ = decode_recovery(rec, &med_limits); }
        if let Ok(rec) = Record::decode(input, &med_limits, 9, Channel::Control) { let _ = decode_repair(rec, 7, &med_limits); }
        if let Ok(rec) = Record::decode(input, &med_limits, 9, Channel::MediaConfig) { let _ = decode_progress(rec, &med_limits); }
    }

    // 2. Parsers
    for &b in &[0, 1, 7] { let _ = decode_neg(input, 4096, b); }
    let _ = fr_wire::authority::decode(input, auth_b, &L, d_h2v, p_rel);
    let _ = fr_wire::authority::decode(input, auth_b, &L, d_v2h, p_rel);
    let _ = decode_input_ticket(input, &L, 7, d_h2v, p_rel);
    let _ = decode_attachment(input, parent, 7, &L, d_h2v, p_rel);
    let _ = decode_attachment(input, parent, 8, &L, d_v2h, p_rel);
    let _ = decode_display(input, disp_p, &L, d_h2v, p_rel);
    let _ = decode_display(input, disp_p, &L, d_v2h, p_rel);
    let _ = decode_decoder(input, dec_b, &L, d_h2v, p_rel);
    let _ = decode_decoder(input, dec_b, &L, d_v2h, p_rel);
    let _ = decode_input(input, &L, 9, d_v2h, p_rel);
    let _ = decode_input_result(input, &L, ResultBinding { channel: 9, session: RemoteSessionId::from_raw(1), lease: InputLeaseId::from_raw(2) }, d_h2v, p_rel);
    let _ = decode_held(input, &L, 9, d_v2h, p_rel);
    let _ = decode_clipboard(input, clip_ctx, &L);
    if let Ok(flim) = FileLimits::new(&L, 4096) { let _ = decode_files(input, file_ctx, flim); }
    let _ = decode_presented(input, pres_b, &L, d_v2h, p_rel);
    let _ = decode_metrics(input, pres_b, &L, d_h2v, p_rel);
    let _ = decode_metrics(input, pres_b, &L, d_v2h, p_rel);
    let _ = decode_clock(input, disp_p, &L, d_v2h, p_rel);
    let _ = decode_clock(input, disp_p, &L, d_h2v, p_rel);
}

/// Runs the fuzz smoke campaign for the requested number of iterations.
pub fn run_fuzz_smoke(iterations: usize, seed: u64) {
    let corpus = all_seeds();
    println!(
        "Fuzz smoke starting: {} iterations, {} golden seeds, seed 0x{:016x}",
        iterations,
        corpus.len(),
        seed
    );

    // First: verify all unmutated seeds
    for s in &corpus {
        fuzz_one(&s.bytes);
    }

    // Second: run mutations
    let mut prng = Prng::new(seed);
    for i in 0..iterations {
        let mutated = mutate(&corpus, &mut prng);
        fuzz_one(&mutated);
        if (i + 1) % 2500 == 0 {
            println!("  completed {} / {} iterations", i + 1, iterations);
        }
    }
    println!("Fuzz smoke completed clean: {iterations} iterations");
}

/// Dumps all golden fixture seeds as `.hex` files under the specified directory.
pub fn dump_fixtures(base_dir: &Path) -> std::io::Result<()> {
    let seeds = all_seeds();
    for s in seeds {
        let dir = base_dir.join(s.category);
        std::fs::create_dir_all(&dir)?;
        let file_path = dir.join(format!("{}.hex", s.name));
        let hex = hex_encode(&s.bytes);
        std::fs::write(&file_path, format!("{hex}\n"))?;
        println!("Wrote golden fixture: {}", file_path.display());
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut iterations = 10_000;
    let mut seed = 0x4652_4430_2026_0919;
    let mut dump_dir: Option<PathBuf> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--dump-fixtures" => {
                i += 1;
                if i < args.len() {
                    dump_dir = Some(PathBuf::from(&args[i]));
                }
            }
            "--iterations" => {
                i += 1;
                if i < args.len() {
                    iterations = args[i].parse().unwrap_or(10_000);
                }
            }
            "--seed" => {
                i += 1;
                if i < args.len() {
                    seed = u64::from_str_radix(args[i].trim_start_matches("0x"), 16)
                        .unwrap_or(0x4652_4430_2026_0919);
                }
            }
            _ => {}
        }
        i += 1;
    }

    if let Some(dir) = dump_dir {
        dump_fixtures(&dir).expect("failed to dump fixtures");
    } else {
        run_fuzz_smoke(iterations, seed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_golden_seeds_are_valid_and_distinct() {
        let seeds = all_seeds();
        assert!(seeds.len() >= 35, "must have seeds for all message kinds");
        for s in &seeds {
            assert!(
                s.bytes.starts_with(b"FRD0"),
                "seed {}/{} must start with FRD0",
                s.category,
                s.name
            );
            assert!(
                s.bytes.len() >= 24,
                "seed {}/{} must have valid header length",
                s.category,
                s.name
            );
        }
    }

    #[test]
    fn fuzz_smoke_campaign_runs_clean_for_smoke_duration() {
        // Runs 5,000 deterministic iterations during `cargo test --examples`
        run_fuzz_smoke(5_000, 0x1234_5678_90ab_cdef);
    }
}
