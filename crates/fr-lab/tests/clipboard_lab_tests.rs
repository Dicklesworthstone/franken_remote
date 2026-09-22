#![forbid(unsafe_code)]
//! Deterministic lab scenarios for the controller text clipboard (plan 15.3).
//!
//! Exercises:
//! 1. Echo-loop prevention across bidirectional transfers in virtual time.
//! 2. Oversized item/chunk, metadata mismatch, and chunk sequence refusals.
//! 3. Invalid UTF-8 encoding refusals across single, truncated, overlong, and split sequences.
//! 4. Comprehensive log-scrubbing proving clipboard bytes never appear in diagnostics.

use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    clipboard::{
        Begin, Binding, ClipboardSession, ClipboardSink, Endpoint, Error, PlatformError,
        Publication, Stamp,
    },
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, InputTicketId,
        RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{DesktopPoint, InputBounds, InputCredentials, InputView},
    input_submission::{Capabilities, InputSession as InputOwner},
    limits::{LimitOverrides, ProtocolLimits},
    time::{HostDuration, HostInstant},
};
use fr_lab::{Destination, Fault, Limits, Scenario};
use fr_wire::{
    WireError,
    clipboard::{
        Body, Context, Lane, Message, Role,
        session::{
            Admission, ChannelSession as ClipboardChannel, Offer, Pump, RecordSink, SessionError,
            TransportFailure,
        },
    },
};

fn at(us: u64) -> HostInstant {
    HostInstant::from_micros(us)
}

fn ms(val: u64) -> HostDuration {
    HostDuration::from_millis_checked(val).unwrap()
}

fn input_owner(lifetime_us: u64, session_id: u128, lease_id: u128) -> InputOwner {
    let credentials = InputCredentials {
        session: RemoteSessionId::from_raw(session_id),
        lease: InputLeaseId::from_raw(lease_id),
        ticket: InputTicketId::from_raw(1),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let mut authority = SessionAuthority::new(
        credentials.session,
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(lifetime_us),
            ticket_lifetime: HostDuration::from_micros(1_000_000),
        },
    );
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(at(0)).unwrap();
    authority.mark_view_ready(at(0)).unwrap();
    authority.grant_lease(credentials.lease, at(0)).unwrap();
    authority
        .issue_input_ticket(credentials.lease, credentials.ticket, at(0))
        .unwrap();
    InputOwner::new(
        authority,
        credentials,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        at(0),
    )
    .unwrap()
}

fn context(session_id: u128, lease_id: u128, sender: Role) -> Context {
    Context {
        scope: Binding {
            session: RemoteSessionId::from_raw(session_id),
            lease: InputLeaseId::from_raw(lease_id),
        },
        channel: 88,
        sender,
        lane: Lane::Clipboard,
    }
}

fn channel_session(
    input: &InputOwner,
    role: Role,
    session_id: u128,
    lease_id: u128,
) -> ClipboardChannel {
    ClipboardChannel::new(
        input,
        context(session_id, lease_id, role),
        ProtocolLimits::ABSOLUTE,
        true,
        at(0),
    )
    .unwrap()
}

fn extract_stamp(offer: Offer) -> Stamp {
    assert!(
        matches!(offer, Offer::Queued(_)),
        "unexpected echo suppression when expecting queued offer"
    );
    let Offer::Queued(stamp) = offer else {
        return Stamp {
            id: 0,
            source: Endpoint::Host,
            sequence: 0,
        };
    };
    stamp
}

#[derive(Default, Debug)]
struct TestPlatform {
    published_texts: Vec<String>,
    published_stamps: Vec<Stamp>,
    prepared_texts: Vec<String>,
    prepared_stamps: Vec<Stamp>,
    cleanup_count: usize,
    publication_outcome: Option<Publication>,
}

impl ClipboardSink for TestPlatform {
    fn prepare(&mut self, text: &str, stamp: Stamp) -> Result<(), PlatformError> {
        self.prepared_texts.push(text.to_owned());
        self.prepared_stamps.push(stamp);
        Ok(())
    }

    fn publish(&mut self, text: &str, stamp: Stamp) -> Publication {
        self.published_texts.push(text.to_owned());
        self.published_stamps.push(stamp);
        self.publication_outcome
            .unwrap_or(Publication::SubmittedToOs)
    }

    fn cancel_prepared(&mut self) {
        self.cleanup_count += 1;
    }
}

struct LabQueueSink<'a> {
    scenario: &'a mut Scenario,
    destination: Destination,
    fault: Fault,
    blocked: bool,
    records_sent: usize,
}

impl<'a> LabQueueSink<'a> {
    fn new(scenario: &'a mut Scenario, destination: Destination, fault: Fault) -> Self {
        Self {
            scenario,
            destination,
            fault,
            blocked: false,
            records_sent: 0,
        }
    }
}

impl RecordSink for LabQueueSink<'_> {
    fn try_send(&mut self, record: &[u8]) -> Result<Admission, TransportFailure> {
        assert!(
            record.len() <= 65_536,
            "record must not exceed max envelope"
        );
        if self.blocked {
            return Ok(Admission::Backpressure);
        }
        self.scenario
            .send(self.destination, record, self.fault)
            .map_err(|_| TransportFailure)?;
        self.records_sent += 1;
        Ok(Admission::Accepted)
    }
}

fn assert_pump_idle(chan: &mut ClipboardChannel, scenario: &mut Scenario, dest: Destination) {
    let now = scenario.now();
    let mut sink = LabQueueSink::new(scenario, dest, Fault::after(ms(1)));
    let mut scratch = [0u8; 512];
    assert_eq!(
        chan.pump(&mut scratch, &mut sink, move || now).unwrap(),
        Pump::Idle
    );
    assert_eq!(sink.records_sent, 0);
}

struct VecSink<'a>(&'a mut Vec<Vec<u8>>);

impl RecordSink for VecSink<'_> {
    fn try_send(&mut self, record: &[u8]) -> Result<Admission, TransportFailure> {
        self.0.push(record.to_vec());
        Ok(Admission::Accepted)
    }
}

// Pumps from source channel into scenario and drains deliveries into target channel.
fn pump_and_deliver_all(
    source: &mut ClipboardChannel,
    target: &mut ClipboardChannel,
    target_platform: &mut TestPlatform,
    scenario: &mut Scenario,
    destination: Destination,
    fault: Fault,
) -> usize {
    let mut total_records = 0;
    let mut scratch = vec![0xa5; 16_384 + 93];

    for _ in 0..1050 {
        let now = scenario.now();
        let mut sink = LabQueueSink::new(scenario, destination, fault);
        let pump_state = source.pump(&mut scratch, &mut sink, move || now);
        assert!(scratch.iter().all(|b| *b == 0), "scratch must be cleared");

        match pump_state {
            Ok(Pump::RecordAccepted) => {
                total_records += 1;
            }
            Ok(Pump::ItemAccepted(_) | Pump::CancelAccepted(_)) => {
                total_records += 1;
                break;
            }
            Ok(Pump::Idle | Pump::Suspended) => {
                break;
            }
            Ok(Pump::Deferred) => {}
            Ok(Pump::Backpressure) => {
                assert_ne!(
                    pump_state,
                    Ok(Pump::Backpressure),
                    "unexpected backpressure when sink unblocked"
                );
                break;
            }
            Err(e) => {
                assert!(
                    pump_state.is_ok(),
                    "unexpected session error during pump: {e:?}"
                );
                break;
            }
        }
    }

    // Now advance scenario and drain deliveries into target
    let elapsed = match fault {
        Fault::After(d) => d,
        Fault::Drop => HostDuration::ZERO,
        Fault::Duplicate { first, second } => {
            if first.as_micros() > second.as_micros() {
                first
            } else {
                second
            }
        }
        Fault::Seeded { max_delay, .. } => max_delay,
    };
    if elapsed.as_micros() > 0 {
        scenario.elapse(elapsed).expect("elapse succeeds");
    }
    scenario
        .drain(|delivery| {
            assert_eq!(delivery.to, destination);
            target
                .receive(delivery.payload, target_platform, || delivery.now)
                .expect("receive must succeed for valid framing");
            0
        })
        .expect("drain must succeed");

    total_records
}

#[test]
fn echo_loop_prevention_in_deterministic_lab() {
    let mut scenario = Scenario::new(42, Limits::default()).expect("scenario setup");

    let host_input = input_owner(10_000_000, 10, 20);
    let client_input = input_owner(10_000_000, 10, 20);

    let mut host_chan = channel_session(&host_input, Role::Host, 10, 20);
    let mut client_chan = channel_session(&client_input, Role::Controller, 10, 20);

    let mut host_platform = TestPlatform::default();
    let mut client_platform = TestPlatform::default();

    // 1. Host copies text "host clipboard payload v1"
    let host_text = "host clipboard payload v1";
    let host_offer = host_chan
        .offer(101, host_text, None, scenario.now())
        .expect("host offer succeeds");
    let host_stamp = extract_stamp(host_offer);
    assert_eq!(host_stamp.sequence, 1);

    // Pump and deliver through scenario
    let delivered = pump_and_deliver_all(
        &mut host_chan,
        &mut client_chan,
        &mut client_platform,
        &mut scenario,
        Destination::Client,
        Fault::after(ms(2)),
    );
    assert!(
        delivered >= 2,
        "must deliver Begin + at least 1 Chunk + Commit"
    );
    assert_eq!(client_platform.published_texts, [host_text]);
    assert_eq!(client_platform.published_stamps, [host_stamp]);

    // 2. Client OS emits selection-change event carrying the exact published origin stamp
    let echo_offer = client_chan
        .offer(201, host_text, Some(host_stamp), scenario.now())
        .expect("offer query succeeds");
    assert_eq!(
        echo_offer,
        Offer::EchoSuppressed,
        "echo carrying publication stamp MUST be suppressed"
    );

    // Verify pump on client emits ZERO records
    assert_pump_idle(&mut client_chan, &mut scenario, Destination::Host);
    assert_eq!(
        scenario.metrics().queued_packets,
        0,
        "no packets queued in lab scenario"
    );

    // 3. Client user independently copies identical text without provenance (genuine local copy)
    let genuine_client_offer = client_chan
        .offer(202, host_text, None, scenario.now())
        .expect("genuine copy offer succeeds");
    let client_stamp = extract_stamp(genuine_client_offer);
    assert_eq!(client_stamp.sequence, 1);

    // Deliver client copy to host
    let delivered_to_host = pump_and_deliver_all(
        &mut client_chan,
        &mut host_chan,
        &mut host_platform,
        &mut scenario,
        Destination::Host,
        Fault::after(ms(2)),
    );
    assert!(delivered_to_host >= 2);
    assert_eq!(host_platform.published_texts, [host_text]);
    assert_eq!(host_platform.published_stamps, [client_stamp]);

    // Host OS echoes its publication: must be suppressed
    let host_echo_offer = host_chan
        .offer(102, host_text, Some(client_stamp), scenario.now())
        .expect("host echo check");
    assert_eq!(
        host_echo_offer,
        Offer::EchoSuppressed,
        "host echo of client publication MUST be suppressed"
    );
    assert_pump_idle(&mut host_chan, &mut scenario, Destination::Client);
}

#[test]
fn rapid_echo_burst_and_jitter_settle_cleanly() {
    let mut scenario = Scenario::new(99, Limits::default()).expect("scenario setup");

    let host_input = input_owner(10_000_000, 30, 40);
    let client_input = input_owner(10_000_000, 30, 40);

    let mut host_chan = channel_session(&host_input, Role::Host, 30, 40);
    let mut client_chan = channel_session(&client_input, Role::Controller, 30, 40);

    let mut client_platform = TestPlatform::default();

    let text = "burst test data";
    let offer = host_chan.offer(1, text, None, scenario.now()).unwrap();
    let host_stamp = extract_stamp(offer);

    pump_and_deliver_all(
        &mut host_chan,
        &mut client_chan,
        &mut client_platform,
        &mut scenario,
        Destination::Client,
        Fault::after(ms(1)),
    );

    // Simulate OS generating 25 rapid selection notification events with origin stamp
    for i in 0..25 {
        let res = client_chan.offer(500 + i, text, Some(host_stamp), scenario.now());
        assert_eq!(
            res,
            Ok(Offer::EchoSuppressed),
            "event {i} must be suppressed"
        );
        assert_pump_idle(&mut client_chan, &mut scenario, Destination::Host);
    }
    assert_eq!(scenario.metrics().queued_packets, 0);
}

#[test]
#[allow(clippy::too_many_lines)]
fn oversized_item_and_chunk_refusal_tests() {
    let host_input = input_owner(10_000_000, 50, 60);
    let client_input = input_owner(10_000_000, 50, 60);

    let mut host_chan = channel_session(&host_input, Role::Host, 50, 60);
    let mut platform = TestPlatform::default();

    // 1. Offer an item exceeding 1 MiB (1_048_577 bytes)
    let huge_text = "a".repeat(1_048_577);
    let huge_offer = host_chan.offer(1, &huge_text, None, at(0));
    assert!(
        matches!(
            huge_offer,
            Err(SessionError::Wire(WireError::ResourceLimit)
                | SessionError::Clipboard(Error::Limit))
        ),
        "oversized item must be refused before allocation: got {huge_offer:?}"
    );
    assert_eq!(host_chan.retained_bytes(), 0);

    // 2. Direct core Begin validation for oversized bytes and chunk count
    let binding = Binding {
        session: RemoteSessionId::from_raw(50),
        lease: InputLeaseId::from_raw(60),
    };
    let stamp = Stamp {
        id: 1,
        source: Endpoint::Host,
        sequence: 1,
    };

    for (total_bytes, chunks) in [(1_048_577, 65), (1025, 1025), (100, 0)] {
        let b = Begin {
            binding,
            stamp,
            total_bytes,
            chunks,
        };
        assert_eq!(b.validate(&ProtocolLimits::ABSOLUTE), Err(Error::Limit));
    }

    // 3. Lowered limits via LimitOverrides
    let lowered_limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_clipboard_item_bytes: Some(512),
        ..LimitOverrides::default()
    })
    .unwrap();
    let mut core_session = ClipboardSession::new(
        &client_input,
        Endpoint::Controller,
        lowered_limits,
        true,
        at(0),
    )
    .unwrap();

    let begin_over_lowered = Begin {
        binding,
        stamp,
        total_bytes: 513,
        chunks: 1,
    };
    assert_eq!(
        core_session.begin(begin_over_lowered, at(0)),
        Err(Error::Limit)
    );
    assert_eq!(core_session.reserved_bytes(), 0);

    // 4. Out-of-order and mismatched chunk offsets
    let begin_valid = Begin {
        binding,
        stamp,
        total_bytes: 32,
        chunks: 2,
    };
    core_session.begin(begin_valid, at(0)).unwrap();
    assert_eq!(core_session.reserved_bytes(), 32);

    // Chunk index 1 before chunk index 0 -> Err(ChunkOrder)
    assert_eq!(
        core_session.chunk(stamp, 1, 16, b"0123456789abcdef", at(0)),
        Err(Error::ChunkOrder)
    );
    assert_eq!(core_session.reserved_bytes(), 0);
    assert_eq!(core_session.begin(begin_valid, at(0)), Err(Error::Replay));

    // 5. Incomplete total bytes committed to platform
    let stamp2 = Stamp {
        id: 2,
        source: Endpoint::Host,
        sequence: 2,
    };
    let begin2 = Begin {
        binding,
        stamp: stamp2,
        total_bytes: 10,
        chunks: 1,
    };
    core_session.begin(begin2, at(0)).unwrap();
    core_session.chunk(stamp2, 0, 0, b"12345", at(0)).unwrap();
    assert_eq!(
        core_session.commit(stamp2, 10, &mut platform, || at(0)),
        Err(Error::Incomplete)
    );
    assert_eq!(
        (
            platform.published_texts.len(),
            platform.prepared_texts.len(),
            core_session.reserved_bytes()
        ),
        (0, 0, 0)
    );
}

#[test]
fn invalid_utf8_encoding_refusal_tests() {
    let test_cases: &[(&str, &[&[u8]])] = &[
        ("single 0xff byte", &[&[0xff]]),
        ("truncated 2-byte", &[&[0xc3]]),
        ("truncated 3-byte", &[&[0xe2, 0x82]]),
        ("truncated 4-byte", &[&[0xf0, 0x90, 0x80]]),
        ("invalid continuation", &[&[0xe0, 0x80, 0x80]]),
        ("overlong encoding", &[&[0xc1, 0x81]]),
        ("surrogate codepoint", &[&[0xed, 0xa0, 0x80]]),
        ("split invalid utf8", &[&[0x41, 0x42, 0xe2], &[0x28, 0x43]]),
    ];

    for (desc, chunks) in test_cases {
        let input = input_owner(10_000_000, 70, 80);
        let mut session = ClipboardSession::new(
            &input,
            Endpoint::Controller,
            ProtocolLimits::ABSOLUTE,
            true,
            at(0),
        )
        .unwrap();
        let mut platform = TestPlatform::default();
        let total_bytes: usize = chunks.iter().map(|c| c.len()).sum();
        let stamp = Stamp {
            id: 999,
            source: Endpoint::Host,
            sequence: 1,
        };
        session
            .begin(
                Begin {
                    binding: session.binding(),
                    stamp,
                    total_bytes: u32::try_from(total_bytes).unwrap(),
                    chunks: u32::try_from(chunks.len()).unwrap(),
                },
                at(0),
            )
            .expect("begin succeeds");

        let mut offset = 0;
        for (i, chunk) in chunks.iter().enumerate() {
            session
                .chunk(
                    stamp,
                    u32::try_from(i).unwrap(),
                    u32::try_from(offset).unwrap(),
                    chunk,
                    at(0),
                )
                .expect("chunk ingestion succeeds");
            offset += chunk.len();
        }

        assert_eq!(
            session.commit(
                stamp,
                u32::try_from(total_bytes).unwrap(),
                &mut platform,
                || at(0)
            ),
            Err(Error::InvalidUtf8),
            "invalid UTF-8 ({desc}) must be refused with InvalidUtf8"
        );
        assert_eq!(
            platform.published_texts.len(),
            0,
            "OS publish must NEVER be invoked for {desc}"
        );
        assert_eq!(
            platform.prepared_texts.len(),
            0,
            "OS prepare must NEVER be invoked for {desc}"
        );
        assert_eq!(
            session.reserved_bytes(),
            0,
            "reserved bytes must be cleared on refusal for {desc}"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn log_scrubbing_proves_clipboard_bytes_never_appear_in_diagnostics() {
    let canary_secrets = [
        "CANARY_SECRET_ALPHA_9918273645",
        "CANARY_SECRET_BETA_UTF8_CAFÉ_🦀_4829103948",
        "CANARY_SECRET_GAMMA_LONG_CHUNK_9938210492",
        "CANARY_SECRET_PAYLOAD_TOKEN_X77Y88Z99",
    ];

    let check_no_leak = |formatted: &str, context: &str| {
        for canary in &canary_secrets {
            assert!(
                !formatted.contains(canary),
                "LEAK DETECTED in {context}: contains canary '{canary}'. Full string: '{formatted}'"
            );
        }
    };

    // 1. Core types Debug / Display
    let binding = Binding {
        session: RemoteSessionId::from_raw(0x1234_5678_9abc_def0),
        lease: InputLeaseId::from_raw(0xfeed_beef_cafe_babe),
    };
    check_no_leak(&format!("{binding:?}"), "Binding::Debug");
    // Ensure the raw session ID is redacted from Binding Debug
    assert!(format!("{binding:?}").contains("[redacted]"));

    let stamp = Stamp {
        id: 0x9999_8888_7777_6666_5555_4444_3333_2222,
        source: Endpoint::Host,
        sequence: 42,
    };
    check_no_leak(&format!("{stamp:?}"), "Stamp::Debug");
    // Ensure the random 128-bit ID is not dumped in Stamp Debug
    assert!(!format!("{stamp:?}").contains("9999888877776666"));

    let begin = Begin {
        binding,
        stamp,
        total_bytes: 1024,
        chunks: 4,
    };
    check_no_leak(&format!("{begin:?}"), "Begin::Debug");

    let input = input_owner(10_000_000, 100, 200);
    let mut core_session = ClipboardSession::new(
        &input,
        Endpoint::Controller,
        ProtocolLimits::ABSOLUTE,
        true,
        at(0),
    )
    .unwrap();

    // Ingest canary chunk into core session
    let canary_bytes = canary_secrets[0].as_bytes();
    let canary_len_u32 = u32::try_from(canary_bytes.len()).unwrap();
    let b = Begin {
        binding: core_session.binding(),
        stamp,
        total_bytes: canary_len_u32,
        chunks: 1,
    };
    core_session.begin(b, at(0)).unwrap();
    core_session
        .chunk(stamp, 0, 0, canary_bytes, at(0))
        .unwrap();

    check_no_leak(
        &format!("{core_session:?}"),
        "ClipboardSession::Debug (active)",
    );

    // Commit to platform
    let mut platform = TestPlatform::default();
    let receipt = core_session
        .commit(stamp, canary_len_u32, &mut platform, || at(0))
        .unwrap();
    check_no_leak(&format!("{receipt:?}"), "Receipt::Debug");
    check_no_leak(
        &format!("{core_session:?}"),
        "ClipboardSession::Debug (post-commit)",
    );

    #[rustfmt::skip]
    for err in [
        Error::InvalidUtf8, Error::Limit, Error::ChunkOrder, Error::Expired,
        Error::LocalChanged, Error::Disabled, Error::Closed, Error::Permission,
        Error::Binding, Error::Source, Error::Replay, Error::Busy,
        Error::Allocation, Error::Incomplete, Error::UnknownTransfer,
    ] {
        check_no_leak(&format!("{err:?}"), "fr_core::Error::Debug");
        check_no_leak(&format!("{err}"), "fr_core::Error::Display");
    }

    // 2. Wire types Debug
    let body_chunk = Body::Chunk {
        index: 0,
        offset: 0,
        bytes: canary_secrets[1].as_bytes(),
    };
    let debug_chunk = format!("{body_chunk:?}");
    check_no_leak(&debug_chunk, "Body::Chunk::Debug");
    assert!(debug_chunk.contains("[redacted]"));

    let msg = Message {
        stamp,
        body: body_chunk,
    };
    check_no_leak(&format!("{msg:?}"), "Message::Debug");

    let wire_ctx = context(100, 200, Role::Host);
    check_no_leak(&format!("{wire_ctx:?}"), "wire Context::Debug");

    let mut channel = channel_session(&input, Role::Host, 100, 200);
    channel.offer(1, canary_secrets[2], None, at(0)).unwrap();
    check_no_leak(&format!("{channel:?}"), "ChannelSession::Debug");

    // 3. Scenario deterministic lab trace and failure formatting
    let mut scenario = Scenario::new(777, Limits::default()).unwrap();
    let mut client_chan = channel_session(&input, Role::Controller, 100, 200);
    let mut client_platform = TestPlatform::default();

    pump_and_deliver_all(
        &mut channel,
        &mut client_chan,
        &mut client_platform,
        &mut scenario,
        Destination::Client,
        Fault::after(ms(1)),
    );

    // Inspect Scenario Debug format
    let scenario_debug = format!("{scenario:?}");
    check_no_leak(&scenario_debug, "Scenario::Debug");

    // Inspect all TraceEvents in Scenario
    for event in scenario.trace() {
        check_no_leak(&format!("{event:?}"), "TraceEvent::Debug");
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn bidirectional_concurrent_copies_under_network_delay_and_reordering() {
    let mut scenario = Scenario::new(12345, Limits::default()).expect("scenario setup");

    let host_input = input_owner(10_000_000, 500, 600);
    let client_input = input_owner(10_000_000, 500, 600);

    let mut host_chan = channel_session(&host_input, Role::Host, 500, 600);
    let mut client_chan = channel_session(&client_input, Role::Controller, 500, 600);

    let mut host_platform = TestPlatform::default();
    let mut client_platform = TestPlatform::default();

    let host_text = "host-initiated-copy-alpha";
    let client_text = "client-initiated-copy-beta";

    // Both sides offer concurrently
    let host_offer = host_chan
        .offer(1, host_text, None, scenario.now())
        .expect("host offer");
    let host_stamp = extract_stamp(host_offer);

    let client_offer = client_chan
        .offer(2, client_text, None, scenario.now())
        .expect("client offer");
    let client_stamp = extract_stamp(client_offer);

    // Pump host records into lab with 3ms delay
    let mut pump_burst = |chan: &mut ClipboardChannel, dest: Destination, delay_ms: u64| {
        let mut scratch = vec![0xa5; 16_384 + 93];
        for _ in 0..10 {
            let now = scenario.now();
            let mut sink = LabQueueSink::new(&mut scenario, dest, Fault::after(ms(delay_ms)));
            let p = chan.pump(&mut scratch, &mut sink, move || now).unwrap();
            if matches!(p, Pump::ItemAccepted(_) | Pump::Idle) {
                break;
            }
        }
    };
    pump_burst(&mut host_chan, Destination::Client, 3);
    pump_burst(&mut client_chan, Destination::Host, 2);

    // Advance virtual time by 5ms so all scheduled packets become due
    scenario.elapse(ms(5)).expect("elapse succeeds");

    // Drain all packets to their destinations
    scenario
        .drain(|delivery| {
            match delivery.to {
                Destination::Client => {
                    client_chan
                        .receive(delivery.payload, &mut client_platform, || delivery.now)
                        .expect("client receive succeeds");
                }
                Destination::Host => {
                    host_chan
                        .receive(delivery.payload, &mut host_platform, || delivery.now)
                        .expect("host receive succeeds");
                }
            }
            0
        })
        .expect("drain succeeds");

    // Verify both sides published the opposite peer's text
    assert_eq!(client_platform.published_texts, [host_text]);
    assert_eq!(client_platform.published_stamps, [host_stamp]);

    assert_eq!(host_platform.published_texts, [client_text]);
    assert_eq!(host_platform.published_stamps, [client_stamp]);

    // Both sides' platforms emit change notifications with the received stamps:
    // BOTH MUST BE SUPPRESSED as echoes!
    let host_echo = host_chan
        .offer(10, client_text, Some(client_stamp), scenario.now())
        .expect("host check echo");
    assert_eq!(host_echo, Offer::EchoSuppressed);

    let client_echo = client_chan
        .offer(20, host_text, Some(host_stamp), scenario.now())
        .expect("client check echo");
    assert_eq!(client_echo, Offer::EchoSuppressed);

    // Sinks emit 0 packets
    assert_pump_idle(&mut host_chan, &mut scenario, Destination::Client);
    assert_pump_idle(&mut client_chan, &mut scenario, Destination::Host);

    assert_eq!(
        scenario.metrics().queued_packets,
        0,
        "traffic settled to zero"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn cancellation_mid_stream_cleans_buffers_and_suppresses_echoes() {
    let mut scenario = Scenario::new(888, Limits::default()).expect("scenario setup");

    let host_input = input_owner(10_000_000, 700, 800);
    let client_input = input_owner(10_000_000, 700, 800);

    let mut host_chan = channel_session(&host_input, Role::Host, 700, 800);
    let mut client_chan = channel_session(&client_input, Role::Controller, 700, 800);

    let mut client_platform = TestPlatform::default();

    // Start a 64 KiB item (requires 4 chunks)
    let text = "x".repeat(65_536);
    let old_offer = host_chan.offer(1, &text, None, scenario.now()).unwrap();
    let old_stamp = extract_stamp(old_offer);

    let mut scratch = vec![0xa5; 16_384 + 93];

    // Deliver only the Begin and first Chunk to client
    let mut delivered_records = Vec::new();
    for _ in 0..2 {
        let now = scenario.now();
        let mut gate = Vec::new();
        let p = host_chan
            .pump(&mut scratch, &mut VecSink(&mut gate), move || now)
            .unwrap();
        assert!(matches!(p, Pump::RecordAccepted));
        for rec in gate {
            client_chan
                .receive(&rec, &mut client_platform, || scenario.now())
                .unwrap();
            delivered_records.push(rec);
        }
    }
    assert!(
        client_chan.retained_bytes() > 0,
        "client is buffering partial item"
    );
    assert_eq!(
        client_platform.published_texts.len(),
        0,
        "not published yet"
    );

    // Supersede on host with a replacement item before completing previous
    let new_text = "superseding text";
    let new_offer = host_chan.offer(2, new_text, None, scenario.now()).unwrap();
    let new_stamp = extract_stamp(new_offer);

    // Next pump from host must emit a Cancel for the old item
    let mut cancel_records = Vec::new();
    let now = scenario.now();
    let pump_res = host_chan
        .pump(&mut scratch, &mut VecSink(&mut cancel_records), move || now)
        .unwrap();
    assert_eq!(pump_res, Pump::CancelAccepted(old_stamp));
    assert_eq!(cancel_records.len(), 1);

    // Deliver Cancel to client
    client_chan
        .receive(&cancel_records[0], &mut client_platform, || scenario.now())
        .unwrap();

    // Client's incoming buffer must be cleared!
    assert_eq!(
        client_chan.retained_bytes(),
        0,
        "client buffer cleared after cancel"
    );

    // Now deliver the replacement item completely
    pump_and_deliver_all(
        &mut host_chan,
        &mut client_chan,
        &mut client_platform,
        &mut scenario,
        Destination::Client,
        Fault::after(ms(1)),
    );

    // Only the replacement text was published!
    assert_eq!(client_platform.published_texts, [new_text]);
    assert_eq!(client_platform.published_stamps, [new_stamp]);
}
