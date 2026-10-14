//! Full native STRING -> UTF-8 wire records -> native STRING, both directions.
//! Two real X servers; the one-record transport and authority clocks are fixtures.
use super::publication::Consumer;
use super::*;
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    clipboard::{Binding, Publication},
    ids::*,
    input::*,
    input_submission::{Capabilities, InputSession},
    time::{HostDuration, HostInstant},
};
use fr_native::clipboard::ClipboardSynchronizer;
use fr_wire::clipboard::{
    Context, Lane, Role,
    session::synchronize::Received,
    session::{Admission, ChannelSession, RecordSink, TransportFailure},
};
use std::{
    io::Read,
    os::{fd::OwnedFd, unix::net::UnixStream},
};

struct Desktop {
    process: Child,
    name: String,
}
impl Desktop {
    fn new() -> Self {
        let (reader, writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let process = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-noreset",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::from(OwnedFd::from(writer)))
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut result = Self {
            process,
            name: String::new(),
        };
        let mut number = String::new();
        BufReader::new(reader.take(16))
            .read_line(&mut number)
            .unwrap();
        assert!(number.ends_with('\n'));
        let number: u16 = number.trim().parse().unwrap();
        result.name = format!(":{number}");
        result
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        let _ = self.process.kill();
        self.process.wait().unwrap();
    }
}
fn at() -> HostInstant {
    HostInstant::from_micros(0)
}
fn owner() -> InputSession {
    let credentials = InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
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
            authorization_lifetime: HostDuration::from_micros(10_000_000),
            ticket_lifetime: HostDuration::from_micros(1_000_000),
        },
    );
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(at()).unwrap();
    authority.mark_view_ready(at()).unwrap();
    authority.grant_lease(credentials.lease, at()).unwrap();
    authority
        .issue_input_ticket(credentials.lease, credentials.ticket, at())
        .unwrap();
    InputSession::new(
        authority,
        credentials,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        at(),
    )
    .unwrap()
}
struct Endpoint {
    // Native and authority owners are retired before the independent X server.
    sync: ClipboardSynchronizer,
    input: InputSession,
    desktop: Desktop,
    next_id: u128,
}
impl Endpoint {
    fn new(sender: Role) -> Self {
        let desktop = Desktop::new();
        let input = owner();
        let context = Context {
            scope: Binding {
                session: RemoteSessionId::from_raw(1),
                lease: InputLeaseId::from_raw(2),
            },
            channel: 77,
            sender,
            lane: Lane::Clipboard,
        };
        let channel =
            ChannelSession::new(&input, context, ProtocolLimits::ABSOLUTE, true, at()).unwrap();
        let sync = ClipboardSynchronizer::new(
            channel,
            open(&desktop.name, false, &ProtocolLimits::ABSOLUTE),
        );
        Self {
            sync,
            input,
            desktop,
            next_id: if sender == Role::Host { 1 } else { 1000 },
        }
    }
    fn poll(&mut self, scratch: &mut [u8], lane: &mut LaneFixture) {
        self.sync
            .poll(scratch, lane, at, || {
                self.next_id += 1;
                Ok(self.next_id)
            })
            .unwrap();
    }
}
#[derive(Default)]
struct LaneFixture {
    record: Option<Vec<u8>>,
    accepted: usize,
    receipts: usize,
}
impl RecordSink for LaneFixture {
    fn try_send(&mut self, bytes: &[u8]) -> Result<Admission, TransportFailure> {
        assert!(bytes.len() <= 65_536);
        if self.record.is_some() {
            return Ok(Admission::Backpressure);
        }
        self.record = Some(bytes.to_vec());
        self.accepted += 1;
        Ok(Admission::Accepted)
    }
}
impl LaneFixture {
    fn deliver(&mut self, target: &mut Endpoint) {
        let Some(bytes) = self.record.as_deref() else {
            return;
        };
        match target.sync.receive(bytes, at).unwrap() {
            Received::Deferred => return,
            Received::Consumed(Some(receipt)) => {
                assert_eq!(receipt.publication, Publication::SubmittedToOs);
                self.receipts += 1;
            }
            Received::Consumed(None) => {}
            _ => panic!("native automatic copy was refused"),
        }
        self.record.take().unwrap().fill(0);
    }
}
impl Drop for LaneFixture {
    fn drop(&mut self) {
        if let Some(bytes) = &mut self.record {
            bytes.fill(0);
        }
    }
}

#[test]
fn automatic_string_copies_cross_two_desktops_and_publish_without_echo_or_manual_reads() {
    let _guard = SERIAL.lock().unwrap();
    if display().is_none() {
        return;
    }
    let mut endpoints = [Endpoint::new(Role::Host), Endpoint::new(Role::Controller)];
    let mut lanes = [LaneFixture::default(), LaneFixture::default()];
    let mut scratch = vec![0; 65_536];
    let step = |endpoints: &mut [Endpoint; 2], lanes: &mut [LaneFixture; 2], scratch: &mut [u8]| {
        endpoints[0].poll(scratch, &mut lanes[0]);
        endpoints[1].poll(scratch, &mut lanes[1]);
        lanes[0].deliver(&mut endpoints[1]);
        lanes[1].deliver(&mut endpoints[0]);
    };
    step(&mut endpoints, &mut lanes, &mut scratch);
    for bytes in [Vec::new(), b"caf\xe9 / \xa3\n".to_vec(), vec![0xe9; 16_385]] {
        let mut owners = Vec::new();
        for source in 0..2 {
            let target = 1 - source;
            let receipt = lanes[source].receipts + 1;
            let peer = Peer::start(
                &endpoints[source].desktop.name,
                json!({"body": bytes, "incremental": bytes.len() > 16_384, "chunk": 8193}),
            );
            let deadline = Instant::now() + Duration::from_secs(3);
            while lanes[source].receipts != receipt {
                step(&mut endpoints, &mut lanes, &mut scratch);
                assert!(
                    Instant::now() < deadline,
                    "automatic native conversion stalled"
                );
                thread::sleep(Duration::from_micros(100));
            }
            let mut consumer =
                Consumer::start(&endpoints[target].desktop.name, json!({"body": bytes}));
            let mut drive = || {
                step(&mut endpoints, &mut lanes, &mut scratch);
            };
            consumer.wait_driven(&mut drive, "TARGETS_STRING");
            if bytes.len() > 16_384 {
                consumer.wait_driven(&mut drive, "INCR");
            }
            consumer.wait_driven(&mut drive, "MATCH");
            consumer.done();
            assert_eq!(peer.requests(), ["UTF8", "STRING"]);
            owners.push(peer);
        }
        let sent = [lanes[0].accepted, lanes[1].accepted];
        for _ in 0..64 {
            step(&mut endpoints, &mut lanes, &mut scratch);
        }
        assert_eq!(
            [lanes[0].accepted, lanes[1].accepted],
            sent,
            "own publication must not echo"
        );
    }
    assert_eq!([lanes[0].receipts, lanes[1].receipts], [3, 3]);
    for endpoint in &endpoints {
        assert_eq!(endpoint.sync.retained_channel_bytes(), 0);
        assert!(endpoint.input.monitor().deadline(at()).is_ok());
    }
}
