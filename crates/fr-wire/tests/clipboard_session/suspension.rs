use super::*;
use fr_core::authority::{AuthorityError, ControlStatus};
use std::sync::{Arc, Mutex};

fn shared_owner() -> (Arc<Mutex<SessionAuthority>>, InputOwner) {
    let c = InputCredentials {
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
    let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(at(0)).unwrap();
    a.require_view_evidence(at(0)).unwrap();
    a.mark_view_ready_until(at(100_000), at(0)).unwrap();
    a.grant_lease(c.lease, at(0)).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, at(0)).unwrap();
    let shared = Arc::new(Mutex::new(a));
    let owner = InputOwner::from_shared_authority(
        shared.clone(),
        c,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        at(0),
    )
    .unwrap();
    (shared, owner)
}
fn resume(shared: &Mutex<SessionAuthority>, owner: &InputOwner) {
    owner.monitor().confirm_suspension_release().unwrap();
    shared
        .lock()
        .unwrap()
        .mark_view_ready_until(at(500_000), at(200_000))
        .unwrap();
}

#[test]
fn real_host_lease_and_both_transfer_cursors_survive_a_bounded_view_pause() {
    let (shared, input) = shared_owner();
    let mut sender = session(&input, Role::Controller);
    let mut receiver = session(&input, Role::Host);
    let id = stamp(sender.offer(4, "safe", None, at(0)).unwrap());
    let deadline = sender.outgoing_deadline();
    let mut gate = Gate::default();
    let mut platform = Platform::default();
    assert_eq!(pump(&mut sender, &mut gate, 0), Pump::RecordAccepted);
    receiver
        .receive(&gate.last, &mut platform, || at(0))
        .unwrap();
    let retained = (sender.retained_bytes(), receiver.retained_bytes());
    assert_eq!(pump(&mut sender, &mut gate, 100_000), Pump::Suspended);
    assert!(!receiver.maintain(at(100_000)).unwrap().enabled);
    assert_eq!(
        input.monitor().status(at(100_000)).unwrap(),
        ControlStatus::Suspended {
            until: at(3_000_000),
            released: false,
        }
    );
    assert_eq!(
        shared
            .lock()
            .unwrap()
            .mark_view_ready_until(at(500_000), at(150_000)),
        Err(AuthorityError::ReleasePending)
    );
    assert_eq!(
        (sender.retained_bytes(), receiver.retained_bytes()),
        retained
    );
    assert_eq!(gate.calls, 1);
    assert_eq!(sender.outgoing_deadline(), deadline);
    assert!(!sender.is_closed());
    assert!(!receiver.is_closed());
    resume(&shared, &input);
    assert_eq!(pump(&mut sender, &mut gate, 200_000), Pump::RecordAccepted);
    receiver
        .receive(&gate.last, &mut platform, || at(200_000))
        .unwrap();
    assert_eq!(
        pump(&mut sender, &mut gate, 200_000),
        Pump::ItemAccepted(id)
    );
    let receipt = receiver
        .receive(&gate.last, &mut platform, || at(200_000))
        .unwrap()
        .unwrap();
    assert_eq!(receipt.publication, Publication::SubmittedToOs);
    assert_eq!(platform.text, ["safe"]);
    assert!(!input.monitor().is_revoked());
}

#[test]
fn second_authority_check_defers_payload_without_advancing_its_cursor() {
    let (shared, input) = shared_owner();
    let mut sender = session(&input, Role::Controller);
    let id = stamp(sender.offer(4, "same", None, at(0)).unwrap());
    let mut gate = Gate::default();
    let mut calls = 0;
    let mut scratch = vec![0xa5; 16_477];
    assert_eq!(
        sender
            .pump(&mut scratch, &mut gate, || {
                calls += 1;
                at(if calls == 1 { 99_999 } else { 100_000 })
            })
            .unwrap(),
        Pump::Deferred
    );
    assert_eq!(gate.calls, 0);
    assert!(scratch.iter().all(|v| *v == 0));
    resume(&shared, &input);
    assert_eq!(pump(&mut sender, &mut gate, 200_000), Pump::RecordAccepted);
    let message = decode(
        &gate.last,
        context(Role::Controller),
        &ProtocolLimits::ABSOLUTE,
    )
    .unwrap();
    assert_eq!(message.stamp, id);
    assert!(matches!(message.body, Body::Begin { .. }));
}

#[test]
fn host_lease_expiry_while_paused_still_retires_the_channel() {
    let (_, input) = shared_owner();
    let mut sender = session(&input, Role::Controller);
    sender.offer(4, "secret", None, at(0)).unwrap();
    assert!(!sender.maintain(at(100_000)).unwrap().enabled);
    assert!(sender.maintain(at(3_000_000)).is_err());
    assert!(sender.is_closed());
    assert_eq!(sender.retained_bytes(), 0);
    assert!(input.monitor().is_revoked());
}

#[test]
fn attachment_and_egress_distinguish_a_held_paused_lease_from_revocation() {
    use fr_wire::clipboard::session::Readiness;

    let (shared, input) = shared_owner();
    let mut channel = ClipboardChannel::new(
        &input,
        context(Role::Host),
        ProtocolLimits::ABSOLUTE,
        true,
        at(100_000),
    )
    .unwrap();
    let transport = channel.transport();
    assert_eq!(
        transport.check(at(100_000)),
        Err(SessionError::Clipboard(Error::Suspended))
    );
    let state = channel.maintain(at(100_000)).unwrap();
    assert!(!state.enabled);
    assert_eq!(state.readiness, Readiness::Suspended);
    assert_eq!(
        channel.offer(7, "waiting", None, at(100_000)),
        Err(SessionError::Clipboard(Error::Suspended))
    );
    assert_eq!(channel.retained_bytes(), 0);
    resume(&shared, &input);
    transport.check(at(200_000)).unwrap();
    channel.offer(7, "ready", None, at(200_000)).unwrap();
    input.revoke_handle().revoke();
    assert!(matches!(
        transport.check(at(200_000)),
        Err(SessionError::Clipboard(Error::Authority(_)))
    ));
}

#[path = "suspension/native.rs"]
mod native;
