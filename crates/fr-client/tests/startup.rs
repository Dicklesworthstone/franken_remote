use fr_client::startup::{Error, Startup};
use fr_core::{
    ids::{HostBootId, OsSessionId, RemoteSessionId},
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::negotiation::{self, Capability, ControlBinding, Message, Offer, Role};
fn offer() -> Offer {
    Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: Role::RequestControl,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities: vec![],
    }
}
fn bytes(m: &Message) -> Vec<u8> {
    let mut b = vec![0; 4096];
    let n = negotiation::encode(m, 4096, &mut b).unwrap();
    b.truncate(n);
    b
}
fn binding() -> ControlBinding {
    ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(11),
        os_session: OsSessionId::from_raw(12),
        remote_session: RemoteSessionId::from_raw(13),
    }
}
fn selected(start: u64) -> Startup {
    let mut c = Startup::new(offer(), start, 10_000).unwrap();
    c.sent(start).unwrap();
    c.receive(&bytes(&Message::HostCapabilities(offer())), start)
        .unwrap();
    c.sent(start).unwrap();
    c
}
fn opened() -> Message {
    Message::SessionOpened {
        binding: binding(),
        selection: offer().select().unwrap(),
        observation_until_us: 17,
    }
}
#[test]
fn actual_startup_records_preserve_bytes_and_require_binding_acknowledgement() {
    let mut c = Startup::new(offer(), 10, 10_000).unwrap();
    let initial = c.pending(10).unwrap().unwrap().to_vec();
    assert_eq!(
        negotiation::decode(&initial, 4096, 0).unwrap(),
        Message::ClientHello(offer())
    );
    assert_eq!(c.pending(200).unwrap().unwrap(), initial);
    assert_eq!(c.deadline_us(), 10_010);
    c.sent(201).unwrap();
    c.receive(&bytes(&Message::HostCapabilities(offer())), 202)
        .unwrap();
    let selection = c.pending(202).unwrap().unwrap().to_vec();
    assert_eq!(
        negotiation::decode(&selection, 4096, 0).unwrap(),
        Message::SelectedConfiguration(offer().select().unwrap())
    );
    c.sent(203).unwrap();
    c.receive(&bytes(&opened()), 204).unwrap();
    assert!(!c.is_complete());
    assert_eq!(c.binding_to_install(205).unwrap(), Some((binding(), 4096)));
    c.bound(7, 206).unwrap();
    assert!(!c.is_complete());
    assert_eq!(
        negotiation::decode(c.pending(206).unwrap().unwrap(), 4096, 7).unwrap(),
        Message::BindingAccepted { binding: 7 }
    );
    c.sent(207).unwrap();
    assert!(c.is_complete());
    assert_eq!(c.finish(208).unwrap().binding, binding());
}
#[test]
fn host_clock_is_not_compared_to_client_clock_and_approval_cannot_renew_timeout() {
    let mut c = selected(10_000_000);
    c.receive(
        &bytes(&Message::ApprovalRequired {
            request: binding().remote_session,
            deadline_us: 1,
            role: Role::RequestControl,
        }),
        10_000_001,
    )
    .unwrap();
    assert_eq!(c.approval().unwrap().host_deadline_us, 1);
    assert_eq!(c.deadline_us(), 10_010_000);
    c.receive(&bytes(&opened()), 10_000_002).unwrap();
    c.bound(7, 10_000_003).unwrap();
    c.sent(10_000_004).unwrap();
    assert_eq!(c.finish(10_000_005).unwrap().observation_until_us, 17);
}
#[test]
fn unexpected_early_or_replayed_messages_are_terminal() {
    let mut c = Startup::new(offer(), 0, 10_000).unwrap();
    assert_eq!(c.receive(&bytes(&opened()), 1), Err(Error::Order));
    assert_eq!(c.pending(2), Err(Error::Closed));
    let mut c = selected(0);
    let approval = bytes(&Message::ApprovalRequired {
        request: binding().remote_session,
        deadline_us: 1,
        role: Role::RequestControl,
    });
    c.receive(&approval, 1).unwrap();
    assert_eq!(c.receive(&approval, 2), Err(Error::Order));
    let mut c = selected(0);
    c.receive(&bytes(&opened()), 1).unwrap();
    assert_eq!(c.bound(8, 2), Err(Error::Order));
}
#[test]
fn approval_cannot_be_retargeted_to_a_different_session_or_role() {
    let mut c = selected(0);
    c.receive(
        &bytes(&Message::ApprovalRequired {
            request: RemoteSessionId::from_raw(99),
            deadline_us: 1,
            role: Role::RequestControl,
        }),
        1,
    )
    .unwrap();
    assert_eq!(c.receive(&bytes(&opened()), 2), Err(Error::Order));
    let mut c = selected(0);
    assert_eq!(
        c.receive(
            &bytes(&Message::ApprovalRequired {
                request: binding().remote_session,
                deadline_us: 1,
                role: Role::Observe
            }),
            1
        ),
        Err(Error::Order)
    );
}
#[test]
fn host_cannot_change_selected_capabilities_or_limits() {
    let mut c = selected(0);
    let mut selection = offer().select().unwrap();
    selection.limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(4096),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        c.receive(
            &bytes(&Message::SessionOpened {
                binding: binding(),
                selection,
                observation_until_us: 9
            }),
            1
        ),
        Err(Error::Order)
    );
    let mut original = offer();
    original.capabilities.push(Capability {
        name: "required".into(),
        version: 1,
        required: true,
    });
    let mut c = Startup::new(original, 0, 100).unwrap();
    c.sent(0).unwrap();
    assert!(
        c.receive(&bytes(&Message::HostCapabilities(offer())), 1)
            .is_err()
    );
    assert_eq!(c.tick(2), Err(Error::Closed));
}
#[test]
fn exact_expiry_clock_regression_and_deadline_overflow_do_not_reopen() {
    assert!(Startup::new(offer(), u64::MAX - 1, 2).is_err());
    assert!(Startup::new(offer(), 0, 0).is_err());
    assert!(Startup::new(offer(), 0, 60_000_001).is_err());
    let mut c = Startup::new(offer(), 10, 100).unwrap();
    assert!(c.pending(109).unwrap().is_some());
    assert_eq!(c.sent(110), Err(Error::Expired));
    assert_eq!(c.tick(11), Err(Error::Closed));
    let mut c = Startup::new(offer(), 10, 100).unwrap();
    assert_eq!(c.tick(9), Err(Error::Clock));
    assert_eq!(c.tick(11), Err(Error::Closed));
    let mut c = selected(0);
    c.receive(&bytes(&opened()), 1).unwrap();
    c.bound(7, 2).unwrap();
    c.sent(3).unwrap();
    assert_eq!(c.finish(10_000), Err(Error::Expired));
}
#[test]
fn malformed_bytes_and_tiny_negotiated_records_never_create_an_opened_session() {
    let mut c = selected(0);
    assert!(c.receive(b"bad", 1).is_err());
    assert_eq!(c.pending(2), Err(Error::Closed));
    let mut o = offer();
    o.limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(128),
        ..Default::default()
    })
    .unwrap();
    let mut c = Startup::new(o.clone(), 0, 100).unwrap();
    c.sent(0).unwrap();
    c.receive(&bytes(&Message::HostCapabilities(o.clone())), 1)
        .unwrap();
    c.sent(2).unwrap();
    assert!(
        c.receive(
            &bytes(&Message::SessionOpened {
                binding: binding(),
                selection: o.select().unwrap(),
                observation_until_us: 3
            }),
            3
        )
        .is_err()
    );
    assert!(!c.is_complete());
}
#[test]
fn diagnostics_do_not_print_pending_capabilities_binding_or_approval_identity() {
    let mut o = offer();
    o.capabilities.push(Capability {
        name: "sensitive-name".into(),
        version: 1,
        required: false,
    });
    let c = Startup::new(o, 0, 100).unwrap();
    assert!(!format!("{c:?}").contains("sensitive-name"));
    let mut c = selected(0);
    c.receive(&bytes(&opened()), 1).unwrap();
    c.bound(7, 2).unwrap();
    c.sent(3).unwrap();
    assert_eq!(
        format!("{:?}", c.finish(4).unwrap()),
        "Opened([negotiated metadata, not input authority])"
    );
}
