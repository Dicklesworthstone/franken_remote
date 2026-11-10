use fr_client::startup::{Error, Startup};
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    directory::{self, Entry, Listing},
    input::InputDelivery,
    inspection,
    negotiation::{self, Message, Offer, Role},
    refusal::{self, Reason, Refused},
};

fn offer() -> Offer {
    directory::prepare_offer(Offer {
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
fn listing(n: u8) -> Listing {
    Listing {
        entries: (0..n)
            .map(|i| Entry {
                name: format!("m{i}.tail1f21e.ts.net"),
                node_id: format!("n{i}"),
                os: "macOS".into(),
                online: true,
                serving: i == 0,
                ipv4: Some(std::net::Ipv4Addr::new(100, 70, 0, i)),
                ipv6: None,
            })
            .collect(),
        truncated: false,
    }
}
/// Hello sent, capabilities received, the request pending.
fn requesting() -> Startup {
    let mut startup = Startup::new(offer(), 10, 30_000_000).unwrap();
    startup.begin_directory(20).unwrap();
    startup.sent(21).unwrap();
    startup
        .receive(&bytes(&Message::HostCapabilities(offer())), 22)
        .unwrap();
    startup
}
fn refused(reason: Reason) -> Vec<u8> {
    let mut out = vec![0; refusal::MAX_BYTES];
    let len = refusal::encode(
        Refused::connection(reason),
        0,
        4096,
        &mut out,
        InputDelivery::Reliable,
    )
    .unwrap();
    out.truncate(len);
    out
}

#[test]
fn one_request_then_the_whole_listing_without_selection_binding_or_approval() {
    let mut startup = requesting();
    assert_eq!(startup.deadline_us(), 5_000_010, "the directory budget");
    let pending = startup.pending(23).unwrap().unwrap().to_vec();
    directory::decode_request(&pending, 4096).unwrap();
    startup.sent(24).unwrap();
    assert!(startup.pending(25).unwrap().is_none());
    let expected = listing(200);
    let records = directory::encode_listing(&expected, 1024).unwrap();
    assert!(records.len() > 1);
    for (i, record) in records.iter().enumerate() {
        assert!(!startup.is_listed());
        startup.receive(record, 30 + i as u64).unwrap();
    }
    assert!(startup.is_listed());
    assert!(!startup.is_complete() && !startup.is_inspected());
    assert!(startup.approval().is_none());
    assert_eq!(startup.binding_to_install(100), Ok(None));
    assert_eq!(startup.finish_directory(101), Ok(expected));
}

#[test]
fn the_hosts_typed_refusals_and_every_disorder_end_the_startup() {
    // Before capabilities (the directory is off) or after the request.
    for reason in [Reason::DirectoryDisabled, Reason::RequiredCapability] {
        let mut startup = Startup::new(offer(), 0, 30_000_000).unwrap();
        startup.begin_directory(0).unwrap();
        startup.sent(1).unwrap();
        assert_eq!(
            startup.receive(&refused(reason), 2),
            Err(Error::Protocol(negotiation::Error::Refused(
                Refused::connection(reason)
            )))
        );
        assert_eq!(startup.pending(3), Err(Error::Closed));
    }
    let mut startup = requesting();
    startup.sent(23).unwrap();
    assert_eq!(
        startup.receive(&refused(Reason::DirectoryUnavailable), 24),
        Err(Error::Protocol(negotiation::Error::Refused(
            Refused::connection(Reason::DirectoryUnavailable)
        )))
    );

    // A listing before the request was handed to the transport, a host that
    // dropped the marker, a gap in the records, and an early finish.
    let records = directory::encode_listing(&listing(200), 1024).unwrap();
    let mut startup = requesting();
    assert!(startup.receive(&records[0], 23).is_err());
    let mut startup = Startup::new(offer(), 0, 30_000_000).unwrap();
    startup.begin_directory(0).unwrap();
    startup.sent(1).unwrap();
    let mut plain = offer();
    plain.capabilities.clear();
    assert_eq!(
        startup.receive(&bytes(&Message::HostCapabilities(plain)), 2),
        Err(Error::Protocol(negotiation::Error::RequiredCapability))
    );
    let mut startup = requesting();
    startup.sent(23).unwrap();
    startup.receive(&records[0], 24).unwrap();
    assert!(startup.receive(&records[2], 25).is_err());
    assert_eq!(startup.pending(26), Err(Error::Closed));
    let mut startup = requesting();
    startup.sent(23).unwrap();
    startup.receive(&records[0], 24).unwrap();
    assert_eq!(startup.finish_directory(25), Err(Error::Order));

    // Only before the hello, once, with the marker, never with inspection.
    let mut startup = Startup::new(offer(), 0, 30_000_000).unwrap();
    startup.sent(1).unwrap();
    assert_eq!(startup.begin_directory(2), Err(Error::Order));
    let mut startup = Startup::new(offer(), 0, 30_000_000).unwrap();
    startup.begin_directory(0).unwrap();
    assert_eq!(startup.begin_directory(1), Err(Error::Order));
    let inspecting = inspection::prepare_offer(Offer {
        capabilities: vec![],
        ..offer()
    })
    .unwrap();
    let mut startup = Startup::new(inspecting, 0, 30_000_000).unwrap();
    assert_eq!(startup.begin_directory(0), Err(Error::Order));

    // The directory deadline counts from creation, parked time included.
    let mut startup = Startup::new(offer(), 0, 30_000_000).unwrap();
    startup.begin_directory(0).unwrap();
    assert_eq!(
        startup.pending(directory::MAX_DURATION_US),
        Err(Error::Expired)
    );
}

#[test]
fn the_native_directory_offer_requires_only_its_marker() {
    let offer = fr_client::native::directory_offer().unwrap();
    assert!(directory::requested(&offer));
    assert!(!inspection::requested(&offer));
    let required: Vec<_> = offer.capabilities.iter().filter(|c| c.required).collect();
    assert_eq!(required.len(), 1);
    assert_eq!(required[0].name, directory::CAPABILITY);
    assert!(offer.capabilities.len() <= negotiation::MAX_CAPABILITIES);
}
