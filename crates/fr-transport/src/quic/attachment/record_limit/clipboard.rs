//! Real selected limits, framing, codec and authority owners. The immediate
//! record admission and publication sinks are fixtures, not QUIC or OS proof.
use super::*;
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    clipboard::{Binding, ClipboardSink, MAX_CHUNK_BYTES, PlatformError, Publication, Stamp},
    ids::*,
    input::*,
    input_submission::{Capabilities, InputSession},
    limits::LimitOverrides,
    time::{HostDuration, HostInstant},
};
use fr_wire::{
    HEADER_BYTES, WireError,
    clipboard::{
        self, Context, Lane, Role,
        session::{Admission, ChannelSession, Offer, Pump, RecordSink, TransportFailure},
    },
    stream::{RecordStream, StreamError},
};

fn at(us: u64) -> HostInstant {
    HostInstant::from_micros(us)
}
fn input() -> InputSession {
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
    authority.authorize_observation(at(0)).unwrap();
    authority.mark_view_ready(at(0)).unwrap();
    authority.grant_lease(credentials.lease, at(0)).unwrap();
    authority
        .issue_input_ticket(credentials.lease, credentials.ticket, at(0))
        .unwrap();
    InputSession::new(
        authority,
        credentials,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        at(0),
    )
    .unwrap()
}
fn context(sender: Role) -> Context {
    Context {
        scope: Binding {
            session: RemoteSessionId::from_raw(1),
            lease: InputLeaseId::from_raw(2),
        },
        channel: 77,
        sender,
        lane: Lane::Clipboard,
    }
}
fn limits(bulk: bool) -> ProtocolLimits {
    let selection = super::negotiation_tests::offer(bulk).select().unwrap();
    let maximum = negotiated_maximum(
        &selection,
        MediaRole::Clipboard,
        65_536,
        8192,
        131_072,
        1150,
    );
    ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(u32::try_from(maximum).unwrap()),
        ..Default::default()
    })
    .unwrap()
}
#[derive(Default)]
struct AdmissionProbe(Vec<u8>);
impl RecordSink for AdmissionProbe {
    fn try_send(&mut self, record: &[u8]) -> Result<Admission, TransportFailure> {
        self.0.clear();
        self.0.extend_from_slice(record);
        Ok(Admission::Accepted)
    }
}
#[derive(Default)]
struct PublicationProbe(Vec<String>);
impl ClipboardSink for PublicationProbe {
    fn prepare(&mut self, _: &str, _: Stamp) -> Result<(), PlatformError> {
        Ok(())
    }
    fn publish(&mut self, text: &str, _: Stamp) -> Publication {
        self.0.push(text.to_owned());
        Publication::SubmittedToOs
    }
}
fn frame(stream: &mut RecordStream, record: &[u8], maximum: usize) {
    // Split the header and UTF-8 arbitrarily, including across code points.
    let mut offset = 0;
    for size in [1, 7, 17, 4093].into_iter().cycle() {
        let end = record.len().min(offset + size);
        assert_eq!(stream.push(&record[offset..end], 0).unwrap(), end - offset);
        offset = end;
        assert!(stream.allocated_bytes() <= maximum);
        if offset == record.len() {
            break;
        }
        assert!(stream.frame(0).unwrap().is_none());
    }
    assert_eq!(stream.frame(0).unwrap().unwrap(), record);
}

#[test]
fn complete_one_mib_unicode_uses_larger_chunks_in_both_directions_without_echo() {
    let text = "🦀".repeat(262_144);
    for bulk in [false, true] {
        let limits = limits(bulk);
        let maximum = limits.max_control_message_bytes() as usize;
        let chunk = (maximum - clipboard::CHUNK_OVERHEAD).min(MAX_CHUNK_BYTES);
        assert_eq!(text.len(), limits.max_clipboard_item_bytes() as usize);
        for role in [Role::Host, Role::Controller] {
            let source_input = input();
            let peer_input = input();
            let mut source =
                ChannelSession::new(&source_input, context(role), limits, true, at(0)).unwrap();
            let reverse = if role == Role::Host {
                Role::Controller
            } else {
                Role::Host
            };
            let mut peer =
                ChannelSession::new(&peer_input, context(reverse), limits, true, at(0)).unwrap();
            let mut framing = RecordStream::new(maximum, 77, 2_000_000).unwrap();
            let mut record = AdmissionProbe::default();
            let mut sink = PublicationProbe::default();
            let mut scratch = vec![0xa5; maximum];
            let Offer::Queued(stamp) = source.offer(1, &text, None, at(0)).unwrap() else {
                panic!("queued item")
            };
            let original_deadline = source.outgoing_deadline().unwrap();
            let records = text.len().div_ceil(chunk) + 2;
            assert_eq!(records, if bulk { 66 } else { 132 });
            for index in 0..records {
                assert_eq!(source.outgoing_deadline(), Some(original_deadline));
                let sent = source.pump(&mut scratch, &mut record, || at(0)).unwrap();
                let last = index == records - 1;
                assert_eq!(
                    sent,
                    if last {
                        Pump::ItemAccepted(stamp)
                    } else {
                        Pump::RecordAccepted
                    }
                );
                assert!(scratch.iter().all(|b| *b == 0));
                assert!(record.0.len() <= maximum);
                frame(&mut framing, &record.0, maximum);
                assert!(sink.0.is_empty(), "publication must wait for Commit");
                let receipt = peer
                    .receive(framing.frame(0).unwrap().unwrap(), &mut sink, || at(0))
                    .unwrap();
                assert_eq!(receipt.is_some(), last);
                framing.consume(0).unwrap();
                assert_eq!(framing.buffered_bytes(), 0);
            }
            assert_eq!(sink.0.as_slice(), std::slice::from_ref(&text));
            assert_eq!(source.retained_bytes(), 0);
            assert_eq!(peer.retained_bytes(), 0);
            assert_eq!(
                peer.offer(2, &text, Some(stamp), at(0)).unwrap(),
                Offer::EchoSuppressed
            );
            assert_eq!(
                peer.pump(&mut scratch, &mut record, || at(0)).unwrap(),
                Pump::Idle
            );
        }
    }
}

#[test]
fn larger_records_cannot_publish_after_the_original_owner_is_revoked() {
    let source_input = input();
    let peer_input = input();
    let limits = limits(true);
    let mut source =
        ChannelSession::new(&source_input, context(Role::Host), limits, true, at(0)).unwrap();
    let mut peer =
        ChannelSession::new(&peer_input, context(Role::Controller), limits, true, at(0)).unwrap();
    let text = "界".repeat(10_000);
    source.offer(3, &text, None, at(0)).unwrap();
    let mut record = AdmissionProbe::default();
    let mut sink = PublicationProbe::default();
    let mut scratch = vec![0; limits.max_control_message_bytes() as usize];
    for _ in 0..3 {
        assert_eq!(
            source.pump(&mut scratch, &mut record, || at(0)).unwrap(),
            Pump::RecordAccepted
        );
        assert!(
            peer.receive(&record.0, &mut sink, || at(0))
                .unwrap()
                .is_none()
        );
    }
    assert!(matches!(
        source.pump(&mut scratch, &mut record, || at(0)).unwrap(),
        Pump::ItemAccepted(_)
    ));
    assert_eq!(peer.retained_bytes(), text.len());
    peer_input.monitor().revoke();
    assert!(peer.receive(&record.0, &mut sink, || at(0)).is_err());
    assert!(peer.is_closed());
    assert_eq!(peer.retained_bytes(), 0);
    assert!(sink.0.is_empty(), "revoked item must not be published");
}

fn header(total: usize) -> [u8; HEADER_BYTES] {
    let mut header = [0; HEADER_BYTES];
    header[..4].copy_from_slice(b"FRD0");
    header[6..8].copy_from_slice(&0x0051_u16.to_be_bytes());
    header[12..16].copy_from_slice(&u32::try_from(total - HEADER_BYTES).unwrap().to_be_bytes());
    header[16..20].copy_from_slice(&77_u32.to_be_bytes());
    header
}
#[test]
fn an_oversized_bulk_header_refuses_before_allocation_even_when_split() {
    for bulk in [false, true] {
        let maximum = limits(bulk).max_control_message_bytes() as usize;
        let header = header(maximum + 1);
        let mut stream = RecordStream::new(maximum, 77, 2_000_000).unwrap();
        for byte in &header[..HEADER_BYTES - 1] {
            assert_eq!(stream.push(&[*byte], 0).unwrap(), 1);
            assert_eq!(stream.allocated_bytes(), 0);
        }
        assert_eq!(
            stream.push(&header[HEADER_BYTES - 1..], 0),
            Err(StreamError::Wire(WireError::ResourceLimit))
        );
        assert_eq!(stream.allocated_bytes(), 0);
        assert_eq!(stream.buffered_bytes(), 0);
    }
}
#[test]
fn full_window_record_keeps_first_byte_expiry_and_releases_storage_on_timeout() {
    let maximum = limits(true).max_control_message_bytes() as usize;
    let mut stream = RecordStream::new(maximum, 77, 2_000_000).unwrap();
    let header = header(maximum);
    stream.push(&header[..1], 100).unwrap();
    assert_eq!(stream.next_deadline(), Some(2_000_100));
    stream.push(&header[1..], 1_000_000).unwrap();
    assert_eq!(stream.allocated_bytes(), maximum);
    stream.push(&[0; 100], 2_000_099).unwrap();
    assert_eq!(stream.next_deadline(), Some(2_000_100));
    assert_eq!(stream.tick(2_000_100), Err(StreamError::Expired));
    assert_eq!(stream.allocated_bytes(), 0);
    assert_eq!(stream.buffered_bytes(), 0);
}
