//! Real native read/publication with actual wire/core composition. The record
//! handoff is an explicitly in-memory all-or-nothing transport fixture, not a
//! claim of live-tailnet transport or a production viewer authority projection.
use super::*;
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    clipboard::{Binding, Error},
    ids::*,
    input::*,
    input_submission::{Capabilities, InputSession as InputOwner},
    time::{HostDuration, HostInstant},
};
use fr_native::clipboard_observation::CopyError;
use fr_wire::clipboard::{
    Context, Lane, Role,
    session::{Admission, ChannelSession, Offer, Pump, RecordSink, SessionError, TransportFailure},
};

fn at(us: u64) -> HostInstant {
    HostInstant::from_micros(us)
}
fn owner() -> InputOwner {
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
    InputOwner::new(
        authority,
        credentials,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        at(0),
    )
    .unwrap()
}
fn channel(input: &InputOwner, sender: Role) -> ChannelSession {
    ChannelSession::new(
        input,
        Context {
            scope: Binding {
                session: RemoteSessionId::from_raw(1),
                lease: InputLeaseId::from_raw(2),
            },
            channel: 77,
            sender,
            lane: Lane::Clipboard,
        },
        ProtocolLimits::ABSOLUTE,
        true,
        at(0),
    )
    .unwrap()
}
#[derive(Default)]
struct Record {
    bytes: Vec<u8>,
}
impl RecordSink for Record {
    fn try_send(&mut self, record: &[u8]) -> Result<Admission, TransportFailure> {
        assert!(record.len() <= 65_536);
        self.bytes.clear();
        self.bytes.extend_from_slice(record);
        Ok(Admission::Accepted)
    }
}
fn copy(
    source: &mut X11Clipboard,
    reader: &mut X11Clipboard,
    channel: &mut ChannelSession,
    id: u128,
) -> Offer {
    let mut read = reader.read_to_channel(channel, id, || at(0)).unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        source.pump().unwrap();
        if let Some(offer) = read.poll(|| at(0)).unwrap() {
            assert_eq!(
                read.poll(|| at(0)),
                Err(CopyError::Native(ReadError::NotReading))
            );
            return offer;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_micros(100));
    }
}

#[test]
fn real_native_selection_crosses_both_authorized_wire_directions_without_echo() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    for text in [
        String::new(),
        "native 🦀 café\n\0tail".to_owned(),
        "🦀".repeat(262_144),
    ] {
        for role in [Role::Host, Role::Controller] {
            let input = owner();
            let peer = owner();
            let mut from = channel(&input, role);
            let mut to = channel(
                &peer,
                if role == Role::Host {
                    Role::Controller
                } else {
                    Role::Host
                },
            );
            let mut source = open(&display);
            let mut reader = open(&display);
            let mut target = open(&display);
            let mut observer = open(&display);
            publish(&mut source, &text, 1);
            let Offer::Queued(stamp) = copy(&mut source, &mut reader, &mut from, 123) else {
                panic!("genuine native copy must queue");
            };
            let mut record = Record::default();
            let mut scratch = vec![0; 65_536];
            let mut complete = false;
            for _ in 0..1030 {
                let state = from.pump(&mut scratch, &mut record, || at(0)).unwrap();
                assert!(scratch.iter().all(|byte| *byte == 0));
                let receipt = to.receive(&record.bytes, &mut target, || at(0)).unwrap();
                if state == Pump::ItemAccepted(stamp) {
                    assert_eq!(receipt.unwrap().publication, Publication::SubmittedToOs);
                    complete = true;
                    break;
                }
                assert_eq!(state, Pump::RecordAccepted);
                assert!(receipt.is_none());
            }
            assert!(complete);
            assert_eq!(from.retained_bytes(), 0);
            observer.begin_read().unwrap();
            assert_eq!(finish(&mut target, &mut observer).unwrap().as_str(), text);
            // The exact remote stamp is retained by the real native publisher.
            // Polling that selection again must not create an echo transfer.
            assert_eq!(
                copy(&mut source, &mut target, &mut to, 456),
                Offer::EchoSuppressed
            );
            assert_eq!(
                to.pump(&mut scratch, &mut record, || at(0)).unwrap(),
                Pump::Idle
            );
            // Equal bytes from a genuine different local owner still propagate.
            publish(&mut source, &text, 2);
            assert!(matches!(
                copy(&mut source, &mut target, &mut to, 789),
                Offer::Queued(_)
            ));
        }
    }
}
#[test]
fn either_off_on_switch_after_native_poll_cancels_without_replay() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    for local in [true, false] {
        let input = owner();
        let mut channel = channel(&input, Role::Host);
        let switch = if local {
            channel.local_switch()
        } else {
            channel.peer_switch()
        };
        let mut source = open(&display);
        let mut reader = open(&display);
        publish(&mut source, &"x".repeat(131_072), 1);
        let mut read = reader.read_to_channel(&mut channel, 123, || at(0)).unwrap();
        source.pump().unwrap();
        let mut calls = 0;
        let result = read.poll(|| {
            calls += 1;
            if calls == 2 {
                switch.set_enabled(false);
                switch.set_enabled(true);
            }
            at(1)
        });
        assert_eq!(
            result,
            Err(CopyError::Session(SessionError::Clipboard(Error::Disabled)))
        );
        assert_eq!(
            read.poll(|| at(2)),
            Err(CopyError::Native(ReadError::NotReading))
        );
        drop(read);
        assert!(matches!(reader.poll_read(), Err(ReadError::NotReading)));
        assert_eq!(channel.retained_bytes(), 0);
        assert!(!channel.is_closed());
        assert!(input.monitor().deadline(at(2)).is_ok());
    }
}
#[test]
fn revoke_expiry_regression_and_caught_poll_panic_retire_native_read() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    for fault in 0..4 {
        let input = owner();
        let mut channel = channel(&input, Role::Host);
        let mut source = open(&display);
        let mut reader = open(&display);
        publish(&mut source, "private", 1);
        let mut read = reader
            .read_to_channel(&mut channel, 123, || at(10))
            .unwrap();
        match fault {
            0 => {
                input.monitor().revoke();
                assert!(read.poll(|| at(11)).is_err());
            }
            1 => {
                assert!(read.poll(|| at(3_000_010)).is_err());
            }
            2 => {
                assert!(read.poll(|| at(9)).is_err());
            }
            _ => {
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _ = read.poll(|| panic!("injected clock fault"));
                    }))
                    .is_err()
                );
            }
        }
        assert_eq!(
            read.poll(|| at(3_000_011)),
            Err(CopyError::Native(ReadError::NotReading))
        );
        drop(read);
        assert!(matches!(reader.poll_read(), Err(ReadError::NotReading)));
        assert_eq!(channel.retained_bytes(), 0);
    }
}
#[test]
fn drop_during_real_incr_clears_requestor_without_revoking_input() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let input = owner();
    let mut channel = channel(&input, Role::Host);
    let mut source = open(&display);
    let mut reader = open(&display);
    publish(&mut source, &"x".repeat(131_072), 1);
    let mut read = reader.read_to_channel(&mut channel, 123, || at(0)).unwrap();
    for _ in 0..100 {
        source.pump().unwrap();
        assert!(read.poll(|| at(0)).unwrap().is_none());
        if source.active_readers() != 0 {
            break;
        }
    }
    assert_eq!(source.active_readers(), 1);
    drop(read);
    source.pump().unwrap();
    assert_eq!(source.active_readers(), 0);
    assert!(matches!(reader.poll_read(), Err(ReadError::NotReading)));
    assert_eq!(channel.retained_bytes(), 0);
    assert!(input.monitor().deadline(at(0)).is_ok());
}
#[test]
fn empty_native_selection_cancels_older_wire_item_instead_of_resuming_it() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let input = owner();
    let mut channel = channel(&input, Role::Host);
    let mut source = open(&display);
    let mut reader = open(&display);
    publish(&mut source, "old", 1);
    let Offer::Queued(stamp) = channel.offer(1, "old", None, at(0)).unwrap() else {
        panic!()
    };
    let mut record = Record::default();
    let mut scratch = vec![0; 65_536];
    assert_eq!(
        channel.pump(&mut scratch, &mut record, || at(0)).unwrap(),
        Pump::RecordAccepted
    );
    source.close();
    assert!(matches!(
        reader.read_to_channel(&mut channel, 123, || at(0)),
        Err(CopyError::Native(ReadError::NoSelection))
    ));
    assert_eq!(channel.retained_bytes(), 0);
    assert_eq!(
        channel.pump(&mut scratch, &mut record, || at(0)).unwrap(),
        Pump::CancelAccepted(stamp)
    );
    assert_eq!(
        channel.pump(&mut scratch, &mut record, || at(0)).unwrap(),
        Pump::Idle
    );
}
#[test]
fn unavailable_owner_and_invalid_id_retire_previously_started_payload() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    for unavailable in [true, false] {
        let input = owner();
        let mut channel = channel(&input, Role::Host);
        let mut source = open(&display);
        let mut reader = open(&display);
        publish(&mut source, "new native selection", 1);
        let Offer::Queued(stamp) = channel.offer(1, "old pending", None, at(0)).unwrap() else {
            panic!()
        };
        let mut scratch = vec![0; 65_536];
        let mut record = Record::default();
        assert_eq!(
            channel.pump(&mut scratch, &mut record, || at(0)).unwrap(),
            Pump::RecordAccepted
        );
        if unavailable {
            reader.close();
        }
        assert!(
            reader
                .read_to_channel(&mut channel, if unavailable { 123 } else { 0 }, || at(0))
                .is_err()
        );
        assert_eq!(channel.retained_bytes(), 0);
        assert_eq!(
            channel.pump(&mut scratch, &mut record, || at(0)).unwrap(),
            Pump::CancelAccepted(stamp)
        );
        assert!(input.monitor().deadline(at(0)).is_ok());
    }
}
#[test]
fn busy_refusal_preserves_existing_read_and_constructor_panic_cleans_new_one() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let input = owner();
    let mut channel = channel(&input, Role::Host);
    let mut source = open(&display);
    let mut reader = open(&display);
    publish(&mut source, "source", 1);
    reader.begin_read().unwrap();
    assert!(matches!(
        reader.read_to_channel(&mut channel, 123, || at(0)),
        Err(CopyError::Native(ReadError::Busy))
    ));
    assert_eq!(finish(&mut source, &mut reader).unwrap().as_str(), "source");
    let mut calls = 0;
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = reader.read_to_channel(&mut channel, 123, || {
                calls += 1;
                assert_ne!(calls, 3, "injected post-native-start clock failure");
                at(0)
            });
        }))
        .is_err()
    );
    assert!(matches!(reader.poll_read(), Err(ReadError::NotReading)));
    assert_eq!(channel.retained_bytes(), 0);
    assert!(matches!(
        copy(&mut source, &mut reader, &mut channel, 456),
        Offer::Queued(_)
    ));
}
