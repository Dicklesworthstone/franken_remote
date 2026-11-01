#![forbid(unsafe_code)]
//! Pure private-worker framing and bounded I/O regressions. These tests make no
//! native-device, process-isolation, clock-correlation or network claims.
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration};
use fr_media::worker::microphone::{
    Configuration, Error, HEADER_BYTES, Header, Incoming, Kind, MAX_AUTHORITY_NS, MAX_BODY_BYTES,
    MAX_RECORD_NS, MAX_SERVER_BYTES, Outgoing, Record, SNAPSHOT_BYTES, Snapshot, Status,
};
use fr_wire::audio::{self, AudioConfiguration};
use std::io::{self, Cursor, ErrorKind, Read, Write};

const ARRIVED: u64 = 1_000_000_000;

fn header(kind: Kind, bytes: usize) -> Header {
    Header {
        kind,
        status: Status::Ok,
        serial: 1,
        generation: 7,
        authorization_ns: ARRIVED + MAX_AUTHORITY_NS,
        arrived_ns: ARRIVED,
        expires_ns: ARRIVED + MAX_RECORD_NS,
        binding: 73,
        bytes: u32::try_from(bytes).unwrap(),
    }
}
fn packet(body: &[u8]) -> Record {
    Record {
        header: header(Kind::Packet, body.len()),
        body: body.to_vec(),
    }
}
fn encoded(record: &Record) -> Vec<u8> {
    let mut bytes = Vec::new();
    record.write(&mut bytes, false).unwrap();
    bytes
}
fn configuration() -> Configuration {
    Configuration {
        server: "/private/pipewire-0".into(),
        packet_maximum: audio::AUDIO_PACKET_OVERHEAD + 1000,
        offer: AudioConfiguration {
            direction: AudioDirection::Uplink,
            generation: AudioGeneration::from_raw(7),
            channels: AudioChannels::Mono,
            sample_rate: 48_000,
            frame_duration_ms: 10,
            max_packet_bytes: 1000,
            max_decoded_samples: 480,
            jitter_target_ms: 20,
        },
    }
}

#[test]
fn every_original_scope_and_custody_field_binds_the_reply() {
    let request = header(Kind::Packet, 3);
    for status in [
        Status::Ok,
        Status::Denied,
        Status::Expired,
        Status::Protocol,
        Status::Native,
        Status::Stopped,
        Status::NotReady,
    ] {
        let reply = request.reply(status, SNAPSHOT_BYTES).unwrap();
        assert!(reply.matches(request));
        assert_eq!(
            Header::decode(&reply.encode(true).unwrap(), true),
            Ok(reply)
        );
        assert_eq!(
            Header::decode(&reply.encode(true).unwrap(), false),
            Err(Error::Malformed)
        );
        if status != Status::Ok {
            assert_eq!(
                Header { status, ..request }.validate(false),
                Err(Error::Malformed)
            );
        }
    }
    let reply = request.reply(Status::Ok, SNAPSHOT_BYTES).unwrap();
    for mismatched in [
        Header {
            kind: Kind::Health,
            ..request
        },
        Header {
            serial: 2,
            ..request
        },
        Header {
            generation: 8,
            ..request
        },
        Header {
            binding: 74,
            ..request
        },
        Header {
            authorization_ns: request.authorization_ns - 1,
            ..request
        },
        Header {
            arrived_ns: request.arrived_ns + 1,
            ..request
        },
        Header {
            expires_ns: request.expires_ns - 1,
            ..request
        },
    ] {
        assert!(
            !reply.matches(mismatched),
            "replacement borrowed an original reply"
        );
    }
}

#[test]
fn header_refuses_noncanonical_fields_and_payloads_on_bodyless_requests() {
    let original = header(Kind::Packet, 3);
    let valid = original.encode(false).unwrap();
    for (offset, value) in [(0, 0), (4, 2), (5, 0), (5, 5), (6, 1), (7, 7)] {
        let mut bytes = valid;
        bytes[offset] = value;
        assert_eq!(Header::decode(&bytes, false), Err(Error::Malformed));
    }
    for offset in 56..HEADER_BYTES {
        let mut bytes = valid;
        bytes[offset] = 1;
        assert_eq!(Header::decode(&bytes, false), Err(Error::Malformed));
    }
    for invalid in [
        Header {
            serial: 0,
            ..original
        },
        Header {
            generation: 0,
            ..original
        },
        Header {
            binding: 0,
            ..original
        },
        Header {
            arrived_ns: 0,
            ..original
        },
        Header {
            expires_ns: ARRIVED,
            ..original
        },
        Header {
            authorization_ns: original.expires_ns - 1,
            ..original
        },
        header(Kind::Commit, 1),
        header(Kind::Health, 1),
    ] {
        assert_eq!(invalid.encode(false), Err(Error::Malformed));
    }
    for kind in [Kind::Commit, Kind::Health] {
        assert!(header(kind, 0).encode(false).is_ok());
    }
}

#[test]
fn packet_receipt_ceiling_and_expiry_are_original_and_inclusive() {
    let original = header(Kind::Packet, 3);
    assert_eq!(original.expires_ns - original.arrived_ns, 100_000_000);
    assert_eq!(original.validate(false), Ok(()));
    assert_eq!(original.live(ARRIVED), Ok(()));
    assert_eq!(original.live(original.expires_ns - 1), Ok(()));
    assert_eq!(original.live(original.expires_ns), Err(Error::Expired));
    assert_eq!(original.live(u64::MAX), Err(Error::Expired));
    assert_eq!(original.live(ARRIVED - 1), Err(Error::Clock));
    assert_eq!(
        Header {
            expires_ns: original.expires_ns + 1,
            ..original
        }
        .validate(false),
        Err(Error::Malformed)
    );
    let clipped = Header {
        authorization_ns: original.expires_ns,
        ..original
    };
    assert_eq!(clipped.live(clipped.authorization_ns - 1), Ok(()));
    assert_eq!(clipped.live(clipped.authorization_ns), Err(Error::Expired));
    let future = Header {
        authorization_ns: ARRIVED + MAX_AUTHORITY_NS + 1,
        ..original
    };
    assert_eq!(future.live(ARRIVED), Err(Error::Clock));
    let renewed = Header {
        authorization_ns: original.authorization_ns + 1,
        ..original
    };
    assert_eq!(renewed.live(original.expires_ns), Err(Error::Expired));
    assert!(
        !renewed
            .reply(Status::Ok, SNAPSHOT_BYTES)
            .unwrap()
            .matches(original)
    );
}

#[test]
fn configuration_requires_the_original_binding_generation_and_complete_body() {
    let selected = configuration();
    let bytes = selected.encode(73).unwrap();
    let original = header(Kind::Configure, bytes.len());
    let decoded = Configuration::decode(original, &bytes).unwrap();
    assert_eq!(decoded.server, selected.server);
    assert_eq!(decoded.offer, selected.offer);
    assert_eq!(decoded.packet_maximum, selected.packet_maximum);
    for wrong in [
        Header {
            binding: 74,
            ..original
        },
        Header {
            generation: 8,
            ..original
        },
        Header {
            serial: 2,
            ..original
        },
        Header {
            kind: Kind::Health,
            ..original
        },
    ] {
        assert_eq!(
            Configuration::decode(wrong, &bytes).unwrap_err(),
            Error::Malformed
        );
    }
    for length in 0..bytes.len() {
        assert_eq!(
            Configuration::decode(original, &bytes[..length]).unwrap_err(),
            Error::Malformed
        );
    }
    for offset in [2, 3, bytes.len() - 1] {
        let mut malformed = bytes.clone();
        malformed[offset] = 255;
        assert_eq!(
            Configuration::decode(original, &malformed).unwrap_err(),
            Error::Malformed
        );
    }
    assert!(!format!("{selected:?}").contains(&selected.server));
}

#[test]
fn configuration_cannot_expand_the_local_source_profile_or_packet_budget() {
    for path in [
        "",
        "/",
        "relative",
        "/private//pipewire-0",
        "/private/../pipewire-0",
        "/private/./pipewire-0",
        "/private/pipewire-0/",
        "/private/pipe wire",
        "/private/pipe\nwire",
        "/private/pipe\u{7f}wire",
    ] {
        let mut selected = configuration();
        selected.server = path.into();
        assert_eq!(selected.validate(), Err(Error::Malformed));
    }
    let mut boundary = configuration();
    boundary.server = format!("/{}", "a".repeat(MAX_SERVER_BYTES - 1));
    assert!(boundary.validate().is_ok());
    boundary.server.push('a');
    assert_eq!(boundary.validate(), Err(Error::Malformed));
    let original = configuration();
    for selected in [
        Configuration {
            packet_maximum: original.packet_maximum - 1,
            ..original.clone()
        },
        Configuration {
            packet_maximum: MAX_BODY_BYTES + 1,
            ..original.clone()
        },
        Configuration {
            offer: AudioConfiguration {
                direction: AudioDirection::Downlink,
                ..original.offer
            },
            ..original.clone()
        },
        Configuration {
            offer: AudioConfiguration {
                generation: AudioGeneration::from_raw(0),
                ..original.offer
            },
            ..original.clone()
        },
        Configuration {
            offer: AudioConfiguration {
                frame_duration_ms: 40,
                ..original.offer
            },
            ..original.clone()
        },
    ] {
        assert_eq!(selected.validate(), Err(Error::Malformed));
    }
    let undersized = Configuration {
        offer: AudioConfiguration {
            max_decoded_samples: 479,
            ..original.offer
        },
        ..original
    };
    assert_eq!(undersized.validate(), Err(Error::Limit));
}

#[test]
fn snapshots_require_canonical_flags_exact_size_and_bounded_native_queues() {
    let original = Snapshot {
        node_id: 17,
        failure_code: 5,
        submitted_frames: u64::MAX,
        silent_frames: u64::MAX - 1,
        expired_frames: u64::MAX - 2,
        queued_frames: 4 * 961,
        queued_packets: 16,
        admitted_packet: true,
    };
    let bytes = original.encode();
    assert_eq!(Snapshot::decode(&bytes), Ok(original));
    for length in 0..SNAPSHOT_BYTES {
        assert_eq!(Snapshot::decode(&bytes[..length]), Err(Error::Malformed));
    }
    assert_eq!(
        Snapshot::decode(&[0; SNAPSHOT_BYTES + 1]),
        Err(Error::Malformed)
    );
    for (offset, value) in [(38, 2), (39, 1)] {
        let mut malformed = bytes;
        malformed[offset] = value;
        assert_eq!(Snapshot::decode(&malformed), Err(Error::Malformed));
    }
    for too_large in [
        Snapshot {
            queued_packets: 17,
            ..original
        },
        Snapshot {
            queued_frames: 4 * 961 + 1,
            ..original
        },
    ] {
        assert_eq!(Snapshot::decode(&too_large.encode()), Err(Error::Limit));
    }
}

struct HeaderOnly {
    bytes: Cursor<[u8; HEADER_BYTES]>,
    calls: usize,
}
impl Read for HeaderOnly {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        assert!(
            self.bytes.position() < u64::try_from(HEADER_BYTES).unwrap(),
            "oversized record requested its body"
        );
        self.calls += 1;
        self.bytes.read(output)
    }
}
#[test]
fn oversized_announcements_refuse_before_any_body_read_or_payload_allocation() {
    for length in [u32::try_from(MAX_BODY_BYTES + 1).unwrap(), u32::MAX] {
        let mut bytes = header(Kind::Packet, 3).encode(false).unwrap();
        bytes[52..56].copy_from_slice(&length.to_be_bytes());
        let mut input = HeaderOnly {
            bytes: Cursor::new(bytes),
            calls: 0,
        };
        assert_eq!(
            Record::read(&mut input, false).unwrap_err(),
            Error::Malformed
        );
        assert_eq!(input.calls, 1);
        let mut input = HeaderOnly {
            bytes: Cursor::new(bytes),
            calls: 0,
        };
        let mut incoming = Incoming::default();
        assert_eq!(
            incoming.poll(&mut input, false).unwrap_err(),
            Error::Malformed
        );
        assert_eq!(input.calls, 1);
    }
    let limit = HEADER_BYTES + MAX_BODY_BYTES + 128;
    assert!(std::mem::size_of::<Incoming>() <= limit);
    assert!(std::mem::size_of::<Outgoing>() <= limit);
}

struct BudgetReader {
    input: Cursor<Vec<u8>>,
    credit: usize,
    chunk: usize,
    calls: usize,
}
impl Read for BudgetReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.calls += 1;
        if self.credit == 0 {
            return Err(ErrorKind::WouldBlock.into());
        }
        let length = output.len().min(self.credit).min(self.chunk);
        let count = self.input.read(&mut output[..length])?;
        self.credit -= count;
        Ok(count)
    }
}
#[test]
fn partial_incoming_turns_keep_the_original_receipt_and_exact_record_boundary() {
    let record = packet(b"a bounded FMPQ payload");
    let mut input = BudgetReader {
        input: Cursor::new(encoded(&record)),
        credit: HEADER_BYTES + 1,
        chunk: 11,
        calls: 0,
    };
    let mut incoming = Incoming::default();
    for _ in 0..8 {
        let before = input.calls;
        assert!(incoming.poll(&mut input, false).unwrap().is_none());
        assert!(input.calls - before <= 4);
        if input.credit == 0 {
            break;
        }
    }
    assert!(incoming.is_partial());
    assert_eq!(incoming.header(), Some(record.header));
    assert_eq!(
        incoming.header().unwrap().live(record.header.expires_ns),
        Err(Error::Expired)
    );
    let before = input.calls;
    assert!(incoming.poll(&mut input, false).unwrap().is_none());
    assert_eq!(input.calls - before, 1);
    input.credit = record.body.len() - 1;
    let complete = incoming.poll(&mut input, false).unwrap().unwrap();
    assert_eq!(complete.header, record.header);
    assert_eq!(complete.body, record.body);
    assert!(!incoming.is_partial());
    assert_eq!(incoming.header(), None);
    assert_eq!(
        input.input.position(),
        u64::try_from(HEADER_BYTES + record.body.len()).unwrap()
    );
}

#[test]
fn malformed_incoming_header_is_never_resynchronized_to_later_magic() {
    let record = packet(b"valid later record");
    let mut bytes = encoded(&record);
    bytes[0] = 0;
    bytes.extend_from_slice(&encoded(&record));
    let mut input = Cursor::new(bytes);
    let mut incoming = Incoming::default();
    assert_eq!(
        incoming.poll(&mut input, false).unwrap_err(),
        Error::Malformed
    );
    assert_eq!(input.position(), u64::try_from(HEADER_BYTES).unwrap());
    assert_eq!(
        incoming.poll(&mut input, false).unwrap_err(),
        Error::Malformed
    );
    assert_eq!(input.position(), u64::try_from(HEADER_BYTES).unwrap());
}

struct Fault {
    kind: Option<ErrorKind>,
    calls: usize,
}
impl Fault {
    fn next(&mut self) -> io::Result<usize> {
        self.calls += 1;
        self.kind.map_or(Ok(0), |kind| Err(kind.into()))
    }
}
impl Read for Fault {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        self.next()
    }
}
impl Write for Fault {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        self.next()
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[test]
fn incoming_interruptions_backpressure_and_io_errors_have_bounded_turns() {
    for kind in [
        Some(ErrorKind::Interrupted),
        Some(ErrorKind::WouldBlock),
        Some(ErrorKind::ConnectionReset),
        None,
    ] {
        let mut input = Fault { kind, calls: 0 };
        let result = Incoming::default().poll(&mut input, false);
        match kind {
            Some(ErrorKind::Interrupted) => {
                assert!(result.unwrap().is_none());
                assert_eq!(input.calls, 4);
            }
            Some(ErrorKind::WouldBlock) => {
                assert!(result.unwrap().is_none());
                assert_eq!(input.calls, 1);
            }
            _ => {
                assert_eq!(result.unwrap_err(), Error::Io);
                assert_eq!(input.calls, 1);
            }
        }
    }
}

#[test]
fn every_truncated_header_and_body_is_terminal_io_not_a_new_receipt() {
    let bytes = encoded(&packet(b"original bounded payload"));
    for length in 0..bytes.len() {
        assert_eq!(
            Record::read(&mut Cursor::new(&bytes[..length]), false).unwrap_err(),
            Error::Io
        );
        let mut incoming = Incoming::default();
        assert_eq!(
            incoming
                .poll(&mut Cursor::new(&bytes[..length]), false)
                .unwrap_err(),
            Error::Io
        );
    }
}

struct BudgetWriter {
    bytes: Vec<u8>,
    credit: usize,
    chunk: usize,
    calls: usize,
}
impl Write for BudgetWriter {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.calls += 1;
        if self.credit == 0 {
            return Err(ErrorKind::WouldBlock.into());
        }
        let count = input.len().min(self.credit).min(self.chunk);
        self.bytes.extend_from_slice(&input[..count]);
        self.credit -= count;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[test]
fn outgoing_partial_writes_preserve_original_bytes_scope_and_expiry() {
    let mut record = packet(b"original immutable speech");
    let original = encoded(&record);
    let original_header = record.header;
    let mut outgoing = Outgoing::new(&record, false).unwrap();
    record.header.authorization_ns += MAX_RECORD_NS;
    record.header.arrived_ns += MAX_RECORD_NS;
    record.header.expires_ns += MAX_RECORD_NS;
    record.body.fill(0x7f);
    let mut output = BudgetWriter {
        bytes: Vec::new(),
        credit: 7,
        chunk: 3,
        calls: 0,
    };
    assert!(!outgoing.poll(&mut output).unwrap());
    assert_eq!(output.calls, 4);
    assert_eq!(output.bytes.len(), 7);
    assert_eq!(outgoing.header(), original_header);
    assert_eq!(
        outgoing.header().live(original_header.expires_ns),
        Err(Error::Expired)
    );
    let before = output.calls;
    assert!(!outgoing.poll(&mut output).unwrap());
    assert_eq!(output.calls - before, 1);
    output.credit = original.len();
    for _ in 0..original.len() {
        let before = output.calls;
        let complete = outgoing.poll(&mut output).unwrap();
        assert!(output.calls - before <= 4);
        assert_eq!(outgoing.header(), original_header);
        if complete {
            break;
        }
    }
    assert_eq!(output.bytes, original);
    let before = output.calls;
    assert!(outgoing.poll(&mut output).unwrap());
    assert_eq!(output.calls, before, "completion cannot replay the record");
}

#[test]
fn outgoing_interruptions_backpressure_and_errors_never_spin_or_replay() {
    let record = packet(b"bounded");
    for kind in [
        Some(ErrorKind::Interrupted),
        Some(ErrorKind::WouldBlock),
        Some(ErrorKind::BrokenPipe),
        None,
    ] {
        let mut output = Fault { kind, calls: 0 };
        let result = Outgoing::new(&record, false).unwrap().poll(&mut output);
        match kind {
            Some(ErrorKind::Interrupted) => {
                assert_eq!(result, Ok(false));
                assert_eq!(output.calls, 4);
            }
            Some(ErrorKind::WouldBlock) => {
                assert_eq!(result, Ok(false));
                assert_eq!(output.calls, 1);
            }
            _ => {
                assert_eq!(result, Err(Error::Io));
                assert_eq!(output.calls, 1);
            }
        }
    }
}

#[test]
fn complete_record_bound_is_accepted_and_mismatched_body_never_writes_a_header() {
    let full = packet(&vec![0x5a; MAX_BODY_BYTES]);
    let encoded = encoded(&full);
    let read = Incoming::default()
        .poll(&mut Cursor::new(&encoded), false)
        .unwrap()
        .unwrap();
    assert_eq!(read.header, full.header);
    assert_eq!(read.body.len(), MAX_BODY_BYTES);
    assert_eq!(read.body, full.body);
    let mut output = Vec::new();
    assert!(
        Outgoing::new(&full, false)
            .unwrap()
            .poll(&mut output)
            .unwrap()
    );
    assert_eq!(output, encoded);
    let mut wrong = packet(b"three");
    wrong.header.bytes -= 1;
    assert!(matches!(
        Outgoing::new(&wrong, false),
        Err(Error::Malformed)
    ));
    let mut empty = Vec::new();
    assert_eq!(wrong.write(&mut empty, false), Err(Error::Malformed));
    assert!(empty.is_empty(), "invalid length leaked a partial header");
}
