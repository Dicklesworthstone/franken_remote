use fr_client::startup::{Error, Startup};
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    inspection,
    negotiation::{self, Message, Offer, Role},
};

fn offer() -> Offer {
    inspection::prepare_offer(Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: Role::Observe,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities: vec![],
    })
    .unwrap()
}
fn bytes(message: &Message) -> Vec<u8> {
    let mut out = vec![0; negotiation::MAX_RECORD];
    let len = negotiation::encode(message, out.len(), &mut out).unwrap();
    out.truncate(len);
    out
}
fn pending_close() -> Startup {
    let mut startup = Startup::new(offer(), 10, 10_000_000).unwrap();
    startup.begin_inspection(20).unwrap();
    startup.sent(21).unwrap();
    startup
        .receive(&bytes(&Message::HostCapabilities(offer())), 22)
        .unwrap();
    startup
}
fn closed() -> Vec<u8> {
    let mut out = vec![0; inspection::CLOSED_BYTES];
    inspection::encode_closed(4096, &mut out).unwrap();
    out
}
#[test]
fn capabilities_require_exact_terminal_ack_without_selection_binding_or_approval() {
    let mut startup = pending_close();
    assert_eq!(startup.deadline_us(), 2_000_010);
    assert!(startup.approval().is_none());
    assert!(!startup.is_complete());
    assert!(!startup.is_inspected());
    assert_eq!(startup.binding_to_install(23), Ok(None));
    let pending = startup.pending(24).unwrap().unwrap().to_vec();
    inspection::decode_request(&pending, 4096).unwrap();
    assert_eq!(startup.pending(500).unwrap().unwrap(), pending);
    startup.sent(501).unwrap();
    assert!(startup.pending(502).unwrap().is_none());
    assert!(!startup.is_inspected());
    startup.receive(&closed(), 503).unwrap();
    assert!(startup.is_inspected());
    assert!(!startup.is_complete());
    assert!(startup.approval().is_none());
    assert_eq!(startup.binding_to_install(504), Ok(None));
    assert_eq!(startup.finish_inspection(505), Ok(offer()));
}
#[test]
fn no_early_report_no_reentry_and_no_fallback_to_observation() {
    assert!(pending_close().finish_inspection(30).is_err());
    let mut startup = pending_close();
    assert_eq!(startup.receive(&closed(), 30), Err(Error::Order));
    assert_eq!(startup.pending(31), Err(Error::Closed));

    let mut startup = Startup::new(offer(), 0, 5_000_000).unwrap();
    startup.sent(1).unwrap();
    assert_eq!(startup.begin_inspection(2), Err(Error::Order));

    let mut missing = offer();
    missing.capabilities.clear();
    let mut startup = Startup::new(offer(), 0, 5_000_000).unwrap();
    startup.begin_inspection(0).unwrap();
    startup.sent(1).unwrap();
    assert_eq!(
        startup.receive(&bytes(&Message::HostCapabilities(missing)), 2),
        Err(Error::Protocol(negotiation::Error::RequiredCapability)),
    );
    assert_eq!(startup.pending(3), Err(Error::Closed));
}
#[test]
fn all_wrong_terminal_claims_fail_and_inspection_cannot_be_a_session() {
    for change in [16, 24, 41, 42, 43, 44, 51] {
        let mut startup = pending_close();
        startup.sent(30).unwrap();
        let mut altered = closed();
        altered[change] ^= 1;
        assert!(startup.receive(&altered, 31).is_err());
        assert!(!startup.is_inspected());
        assert_eq!(startup.pending(32), Err(Error::Closed));
    }
    let mut startup = pending_close();
    startup.sent(30).unwrap();
    startup.receive(&closed(), 31).unwrap();
    assert_eq!(startup.finish(32), Err(Error::Order));
}
#[test]
fn parked_creation_and_partial_close_share_one_original_deadline() {
    let mut startup = Startup::new(offer(), 10, 10_000_000).unwrap();
    assert_eq!(startup.begin_inspection(2_000_010), Err(Error::Expired));
    assert_eq!(startup.pending(2_000_011), Err(Error::Closed));

    let mut startup = pending_close();
    let original = startup.deadline_us();
    startup.pending(original - 2).unwrap();
    startup.sent(original - 1).unwrap();
    assert_eq!(startup.receive(&closed(), original), Err(Error::Expired));
    assert!(!startup.is_inspected());

    let mut startup = Startup::new(offer(), 100, 1000).unwrap();
    startup.begin_inspection(200).unwrap();
    assert_eq!(startup.deadline_us(), 1100);
}
