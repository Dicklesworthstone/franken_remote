//! Actual core/wire composition with explicitly recording transport/OS fixtures.
//! This is not native clipboard or live network qualification.
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    clipboard::{Binding, ClipboardSink, Error, PlatformError, Publication, Stamp},
    ids::*,
    input::*,
    input_submission::{Capabilities, InputSession},
    limits::ProtocolLimits,
    time::{HostDuration, HostInstant},
};
use fr_wire::clipboard::{
    Body, CancelReason, Context, Lane, Role, decode,
    session::{Admission, ChannelSession, Offer, Pump, RecordSink, SessionError, TransportFailure},
};

fn at(us: u64) -> HostInstant {
    HostInstant::from_micros(us)
}
fn owner(lifetime: u64) -> InputSession {
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
            authorization_lifetime: HostDuration::from_micros(lifetime),
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
fn session(input: &InputSession, role: Role) -> ChannelSession {
    ChannelSession::new(input, context(role), ProtocolLimits::ABSOLUTE, true, at(0)).unwrap()
}
fn stamp(offer: Offer) -> Stamp {
    let Offer::Queued(stamp) = offer else {
        panic!("expected a new local item")
    };
    stamp
}
#[derive(Default)]
struct Platform {
    text: Vec<String>,
    outcome: Option<Publication>,
}
impl ClipboardSink for Platform {
    fn prepare(&mut self, _: &str, _: Stamp) -> Result<(), PlatformError> {
        Ok(())
    }
    fn publish(&mut self, text: &str, _: Stamp) -> Publication {
        self.text.push(text.to_owned());
        self.outcome.unwrap_or(Publication::SubmittedToOs)
    }
}
#[derive(Default)]
struct Gate {
    last: Vec<u8>,
    blocked: bool,
    calls: usize,
}
impl RecordSink for Gate {
    fn try_send(&mut self, record: &[u8]) -> Result<Admission, TransportFailure> {
        assert!(record.len() <= 65_536);
        self.calls += 1;
        self.last = record.to_vec();
        Ok(if self.blocked {
            Admission::Backpressure
        } else {
            Admission::Accepted
        })
    }
}
fn pump(session: &mut ChannelSession, gate: &mut Gate, now: u64) -> Pump {
    let mut scratch = vec![0xa5; 16_384 + 93];
    let result = session.pump(&mut scratch, gate, || at(now)).unwrap();
    assert!(scratch.iter().all(|b| *b == 0));
    result
}
fn deliver(from: &mut ChannelSession, to: &mut ChannelSession, platform: &mut Platform) -> Vec<u8> {
    let mut gate = Gate::default();
    for _ in 0..1030 {
        let state = pump(from, &mut gate, 0);
        assert!(matches!(
            state,
            Pump::RecordAccepted | Pump::ItemAccepted(_)
        ));
        to.receive(&gate.last, platform, || at(0)).unwrap();
        if matches!(state, Pump::ItemAccepted(_)) {
            return gate.last;
        }
    }
    panic!("bounded item did not complete");
}

#[test]
fn both_directions_empty_unicode_and_one_mib_publish_without_echo() {
    for text in [
        String::new(),
        "private 🦀 café\n\0tail".to_owned(),
        "🦀".repeat(262_144),
    ] {
        for role in [Role::Host, Role::Controller] {
            let input = owner(10_000_000);
            let peer_input = owner(10_000_000);
            let mut source = session(&input, role);
            let mut target = session(
                &peer_input,
                if role == Role::Host {
                    Role::Controller
                } else {
                    Role::Host
                },
            );
            let mut platform = Platform::default();
            let offered = stamp(source.offer(123, &text, None, at(0)).unwrap());
            assert!(source.retained_bytes() <= 1_048_576);
            deliver(&mut source, &mut target, &mut platform);
            assert_eq!(platform.text.as_slice(), std::slice::from_ref(&text));
            assert_eq!(source.retained_bytes(), 0);
            assert_eq!(target.retained_bytes(), 0);
            assert_eq!(
                target.offer(456, &text, Some(offered), at(0)),
                Ok(Offer::EchoSuppressed)
            );
            assert_eq!(pump(&mut target, &mut Gate::default(), 0), Pump::Idle);
            let genuine_copy = stamp(target.offer(789, &text, None, at(0)).unwrap());
            assert_eq!(genuine_copy.sequence, 1);
        }
    }
}
#[test]
fn backpressure_preserves_exact_record_and_does_not_consume_it() {
    let input = owner(10_000_000);
    let mut source = session(&input, Role::Host);
    source.offer(1, "text", None, at(0)).unwrap();
    let mut gate = Gate {
        blocked: true,
        ..Gate::default()
    };
    for stage in 0..3 {
        assert_eq!(pump(&mut source, &mut gate, 0), Pump::Backpressure);
        let original = gate.last.clone();
        assert_eq!(pump(&mut source, &mut gate, 0), Pump::Backpressure);
        assert_eq!(gate.last, original);
        gate.blocked = false;
        let accepted = pump(&mut source, &mut gate, 0);
        assert_eq!(gate.last, original);
        assert!(if stage < 2 {
            accepted == Pump::RecordAccepted
        } else {
            matches!(accepted, Pump::ItemAccepted(_))
        });
        gate.blocked = true;
    }
    assert_eq!(source.retained_bytes(), 0);
}
#[test]
fn supersession_cancels_started_item_before_only_the_latest_replacement() {
    let input = owner(10_000_000);
    let peer_input = owner(10_000_000);
    let mut source = session(&input, Role::Host);
    let mut target = session(&peer_input, Role::Controller);
    let mut platform = Platform::default();
    let old = stamp(source.offer(1, "old", None, at(0)).unwrap());
    let mut gate = Gate::default();
    assert_eq!(pump(&mut source, &mut gate, 0), Pump::RecordAccepted);
    target.receive(&gate.last, &mut platform, || at(0)).unwrap();
    source.offer(2, "intermediate", None, at(0)).unwrap();
    let latest = stamp(source.offer(3, "latest", None, at(0)).unwrap());
    assert_eq!(latest.sequence, 3);
    assert_eq!(source.retained_bytes(), "latest".len());
    gate.blocked = true;
    assert_eq!(pump(&mut source, &mut gate, 0), Pump::Backpressure);
    let cancel = decode(&gate.last, context(Role::Host), &ProtocolLimits::ABSOLUTE).unwrap();
    assert_eq!(cancel.stamp, old);
    assert_eq!(cancel.body, Body::Cancel(CancelReason::Superseded));
    gate.blocked = false;
    assert_eq!(pump(&mut source, &mut gate, 0), Pump::CancelAccepted(old));
    target.receive(&gate.last, &mut platform, || at(0)).unwrap();
    deliver(&mut source, &mut target, &mut platform);
    assert_eq!(platform.text, ["latest"]);
}
#[test]
fn final_authority_check_refuses_after_encoding_and_clears_scratch() {
    let input = owner(10_000_000);
    let revoke = input.revoke_handle();
    let mut source = session(&input, Role::Host);
    source.offer(1, "secret", None, at(0)).unwrap();
    let mut gate = Gate::default();
    let mut scratch = [0xa5; 1024];
    let mut clocks = 0;
    let result = source.pump(&mut scratch, &mut gate, || {
        clocks += 1;
        if clocks == 2 {
            revoke.revoke();
        }
        at(0)
    });
    assert!(matches!(
        result,
        Err(SessionError::Clipboard(Error::Authority(_)))
    ));
    assert_eq!(gate.calls, 0);
    assert!(source.is_closed());
    assert_eq!(source.retained_bytes(), 0);
    assert!(scratch.iter().all(|b| *b == 0));
}
#[test]
fn fixed_item_expiry_during_silence_does_not_close_live_controller() {
    let input = owner(10_000_000);
    let mut source = session(&input, Role::Host);
    source.offer(1, "secret", None, at(0)).unwrap();
    assert_eq!(source.outgoing_deadline(), Some(at(3_000_000)));
    assert!(!source.maintain(at(2_999_999)).unwrap().outgoing_expired);
    let state = source.maintain(at(3_000_000)).unwrap();
    assert!(state.outgoing_expired && state.enabled);
    assert!(!source.is_closed());
    assert_eq!(source.retained_bytes(), 0);
    assert!(input.monitor().deadline(at(3_000_000)).is_ok());
}
#[test]
fn duplicate_remote_commit_never_discards_a_newer_genuine_local_copy() {
    let input = owner(10_000_000);
    let peer_input = owner(10_000_000);
    let mut source = session(&input, Role::Host);
    let mut target = session(&peer_input, Role::Controller);
    source.offer(1, "remote", None, at(0)).unwrap();
    let mut target_platform = Platform::default();
    let commit = deliver(&mut source, &mut target, &mut target_platform);
    target.offer(2, "new local", None, at(0)).unwrap();
    assert!(
        target
            .receive(&commit, &mut target_platform, || at(0))
            .unwrap()
            .is_some()
    );
    assert_eq!(target_platform.text, ["remote"]);
    assert_eq!(target.retained_bytes(), "new local".len());
    let mut source_platform = Platform::default();
    deliver(&mut target, &mut source, &mut source_platform);
    assert_eq!(source_platform.text, ["new local"]);
}

#[path = "clipboard_session/faults.rs"]
mod faults;
