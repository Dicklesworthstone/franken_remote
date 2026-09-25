//! Actual startup bytes for the profile used by the desktop executable.
use fr_client::{
    native::{control_offer, observation_offer},
    startup::{Error, Startup},
};
use fr_core::ids::{HostBootId, OsSessionId, RemoteSessionId};
use fr_wire::{
    attachment, clock, control, decoder, display,
    negotiation::{self, ControlBinding, Message, Offer, Role, Selection},
    presented, receiver_metrics, recovery_request,
};

fn bytes(message: &Message) -> Vec<u8> {
    let mut bytes = vec![0; negotiation::MAX_RECORD];
    let n = negotiation::encode(message, negotiation::MAX_RECORD, &mut bytes).unwrap();
    bytes.truncate(n);
    bytes
}
fn binding() -> ControlBinding {
    ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(11),
        os_session: OsSessionId::from_raw(12),
        remote_session: RemoteSessionId::from_raw(13),
    }
}
fn selection(host: &Offer) -> (Startup, Selection) {
    let host = host.intersect(&observation_offer()).unwrap();
    let mut startup = Startup::new(observation_offer(), 10, 10_000).unwrap();
    assert_eq!(
        negotiation::decode(
            startup.pending(10).unwrap().unwrap(),
            negotiation::MAX_RECORD,
            0
        )
        .unwrap(),
        Message::ClientHello(observation_offer())
    );
    startup.sent(11).unwrap();
    startup
        .receive(&bytes(&Message::HostCapabilities(host)), 12)
        .unwrap();
    let Message::SelectedConfiguration(selected) = negotiation::decode(
        startup.pending(13).unwrap().unwrap(),
        negotiation::MAX_RECORD,
        0,
    )
    .unwrap() else {
        panic!("expected selected configuration")
    };
    startup.sent(14).unwrap();
    (startup, selected)
}
fn opened(selection: Selection) -> Message {
    Message::SessionOpened {
        binding: binding(),
        selection,
        observation_until_us: 1_000_000,
    }
}
#[test]
fn shipped_profile_is_observation_only_with_mandatory_bootstrap_and_optional_recovery() {
    let offer = observation_offer();
    offer.validate().unwrap();
    assert_eq!(offer.role, Role::Observe);
    assert_eq!(offer.capabilities.len(), 6);
    for name in [
        display::CAPABILITY,
        decoder::CAPABILITY,
        attachment::CAPABILITY,
        attachment::DELIVERY_CAPABILITY,
    ] {
        let cap = offer.capabilities.iter().find(|c| c.name == name).unwrap();
        assert!(cap.required);
        assert_eq!(cap.version, 1);
    }
    for name in [receiver_metrics::CAPABILITY, recovery_request::CAPABILITY] {
        let cap = offer.capabilities.iter().find(|c| c.name == name).unwrap();
        assert!(!cap.required);
        assert_eq!(cap.version, 1);
    }
    assert!(offer.capabilities.iter().all(|c| !c.name.contains("input")
        && !c.name.contains("clipboard")
        && !c.name.contains("audio")
        && !c.name.contains("file")));
}
#[test]
fn all_four_optional_intersections_complete_the_real_startup_without_a_control_grant() {
    for metrics in [false, true] {
        for recovery in [false, true] {
            let mut host = observation_offer();
            host.capabilities.retain(|c| {
                (metrics || c.name != receiver_metrics::CAPABILITY)
                    && (recovery || c.name != recovery_request::CAPABILITY)
            });
            let (mut startup, selected) = selection(&host);
            assert_eq!(selected.role, Role::Observe);
            assert_eq!(
                selected
                    .capabilities
                    .iter()
                    .any(|c| c.name == receiver_metrics::CAPABILITY),
                metrics
            );
            assert_eq!(
                selected
                    .capabilities
                    .iter()
                    .any(|c| c.name == recovery_request::CAPABILITY),
                recovery
            );
            startup
                .receive(&bytes(&opened(selected.clone())), 15)
                .unwrap();
            assert!(!startup.is_complete());
            startup.bound(7, 16).unwrap();
            startup.sent(17).unwrap();
            assert_eq!(startup.finish(18).unwrap().selection, selected);
        }
    }
}
#[test]
fn incompatible_optional_versions_are_not_selected_or_emitted_as_supported() {
    let mut host = observation_offer();
    for cap in host.capabilities.iter_mut().filter(|c| !c.required) {
        cap.version += 1;
    }
    let (_, selected) = selection(&host);
    assert_eq!(selected.capabilities.len(), 4);
    host.capabilities
        .iter_mut()
        .find(|c| c.name == recovery_request::CAPABILITY)
        .unwrap()
        .required = true;
    let mut startup = Startup::new(observation_offer(), 0, 10_000).unwrap();
    startup.sent(1).unwrap();
    assert_eq!(
        startup.receive(&bytes(&Message::HostCapabilities(host)), 2),
        Err(Error::Protocol(negotiation::Error::RequiredCapability))
    );
    assert_eq!(startup.tick(3), Err(Error::Closed));
}
#[test]
fn mandatory_bootstrap_cannot_be_silently_downgraded() {
    for omitted in [
        display::CAPABILITY,
        decoder::CAPABILITY,
        attachment::CAPABILITY,
        attachment::DELIVERY_CAPABILITY,
    ] {
        let mut host = observation_offer();
        host.capabilities.retain(|c| c.name != omitted);
        let mut startup = Startup::new(observation_offer(), 0, 10_000).unwrap();
        startup.sent(1).unwrap();
        assert_eq!(
            startup.receive(&bytes(&Message::HostCapabilities(host)), 2),
            Err(Error::Protocol(negotiation::Error::RequiredCapability))
        );
        assert_eq!(startup.tick(3), Err(Error::Closed));
    }
}
#[test]
fn session_open_cannot_inject_an_unselected_recovery_feature_or_change_observation_role() {
    let mut legacy = observation_offer();
    legacy.capabilities.retain(|c| c.required);
    let (mut startup, _) = selection(&legacy);
    assert_eq!(
        startup.receive(&bytes(&opened(observation_offer().select().unwrap())), 15),
        Err(Error::Order)
    );
    let (mut startup, mut selected) = selection(&observation_offer());
    selected.role = Role::RequestControl;
    assert_eq!(
        startup.receive(&bytes(&opened(selected)), 15),
        Err(Error::Order)
    );
}
#[test]
fn optional_features_cannot_refresh_the_original_startup_deadline() {
    let (mut startup, selected) = selection(&observation_offer());
    assert_eq!(startup.deadline_us(), 10_010);
    assert_eq!(
        startup.receive(&bytes(&opened(selected)), 10_010),
        Err(Error::Expired)
    );
    assert_eq!(startup.tick(20), Err(Error::Closed));
}
#[test]
fn control_profile_adds_only_mandatory_control_boundaries_and_completes_startup() {
    let offer = control_offer();
    offer.validate().unwrap();
    assert_eq!(offer.role, Role::RequestControl);
    let observation = observation_offer();
    for cap in &observation.capabilities {
        assert!(offer.capabilities.contains(cap), "{}", cap.name);
    }
    let added: Vec<_> = offer
        .capabilities
        .iter()
        .filter(|c| !observation.capabilities.contains(c))
        .collect();
    assert_eq!(added.len(), 4);
    for (name, version) in [
        (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
        (control::GRANT_CAPABILITY, 1),
        (clock::CAPABILITY, clock::VERSION),
        (presented::CAPABILITY, presented::VERSION),
    ] {
        assert!(
            added
                .iter()
                .any(|c| c.name == name && c.version == version && c.required),
            "{name}"
        );
    }
    assert!(
        offer
            .capabilities
            .iter()
            .all(|c| !c.name.contains("clipboard")
                && !c.name.contains("audio")
                && !c.name.contains("file"))
    );
    // The real client startup against a control-capable host intersection.
    let host = control_offer().intersect(&control_offer()).unwrap();
    let mut startup = Startup::new(control_offer(), 10, 10_000).unwrap();
    assert_eq!(
        negotiation::decode(
            startup.pending(10).unwrap().unwrap(),
            negotiation::MAX_RECORD,
            0
        )
        .unwrap(),
        Message::ClientHello(control_offer())
    );
    startup.sent(11).unwrap();
    startup
        .receive(&bytes(&Message::HostCapabilities(host)), 12)
        .unwrap();
    let Message::SelectedConfiguration(selected) = negotiation::decode(
        startup.pending(13).unwrap().unwrap(),
        negotiation::MAX_RECORD,
        0,
    )
    .unwrap() else {
        panic!("expected selected configuration")
    };
    startup.sent(14).unwrap();
    assert_eq!(selected.role, Role::RequestControl);
    assert!(
        selected
            .capabilities
            .iter()
            .any(|c| c.name == control::GRANT_CAPABILITY && c.required)
    );
    startup
        .receive(&bytes(&opened(selected.clone())), 15)
        .unwrap();
    startup.bound(7, 16).unwrap();
    startup.sent(17).unwrap();
    assert_eq!(startup.finish(18).unwrap().selection, selected);
}
#[test]
fn observation_only_host_refuses_control_intent_as_a_typed_capability_error() {
    assert!(matches!(
        observation_offer().intersect(&control_offer()),
        Err(negotiation::Error::RequiredCapability)
    ));
    let mut host = observation_offer();
    host.role = Role::RequestControl;
    let mut startup = Startup::new(control_offer(), 0, 10_000).unwrap();
    startup.sent(1).unwrap();
    assert_eq!(
        startup.receive(&bytes(&Message::HostCapabilities(host)), 2),
        Err(Error::Protocol(negotiation::Error::RequiredCapability))
    );
    assert_eq!(startup.tick(3), Err(Error::Closed));
}
