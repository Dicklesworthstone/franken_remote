use fr_client::{
    clock::{ClockExchange, Error},
    input::ClientInstant,
};
use fr_core::{
    ids::{HostBootId, OsSessionId, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_media::freshness::ClockPolicy;
use fr_wire::{
    clock::{self, Message},
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
};
fn binding() -> ControlBinding {
    ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(1),
        os_session: OsSessionId::from_raw(2),
        remote_session: RemoteSessionId::from_raw(3),
    }
}
fn client() -> ClockExchange {
    ClockExchange::new(
        binding(),
        ProtocolLimits::ABSOLUTE,
        ClockPolicy {
            max_exchange_us: 1000,
            valid_for_us: 10_000,
            drift_ppm: 0,
        },
        ClientInstant(0),
    )
    .unwrap()
}
fn reply(sequence: u64, host_sample_us: u64) -> Vec<u8> {
    let mut bytes = [0; clock::REPLY_BYTES];
    let n = clock::encode(
        Message::Reply {
            sequence,
            host_sample_us,
        },
        binding(),
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes[..n].to_vec()
}
#[test]
fn queue_delay_and_asymmetric_network_are_included_without_clock_origin_assumptions() {
    let mut c = client();
    assert!(c.correlation(ClientInstant(0)).unwrap().is_none());
    c.begin(ClientInstant(100)).unwrap();
    let first = c.pending(ClientInstant(100)).unwrap().unwrap().0.to_vec();
    let (again, until) = c.pending(ClientInstant(400)).unwrap().unwrap();
    assert_eq!(first, again);
    assert_eq!(until, 1100);
    c.queued(ClientInstant(500)).unwrap();
    assert!(c.pending(ClientInstant(600)).unwrap().is_none());
    let host = 9_000_000_000;
    let result = c.accept(&reply(1, host), ClientInstant(900)).unwrap();
    // All 800 us count. Neither enqueue success nor callback receipt resets t0.
    assert_eq!(result.age_upper_us(host, 900).unwrap(), 800);
    assert_eq!(result.age_upper_us(host - 20, 1000).unwrap(), 920);
    assert_eq!(result.valid_until_us(), 10_900);
}
#[test]
fn unsent_early_replayed_foreign_and_unsolicited_replies_never_establish_correlation() {
    let mut c = client();
    assert_eq!(
        c.accept(&reply(1, 100), ClientInstant(1)).unwrap_err(),
        Error::UnexpectedReply
    );
    let mut c = client();
    c.begin(ClientInstant(0)).unwrap();
    assert_eq!(
        c.accept(&reply(1, 100), ClientInstant(1)).unwrap_err(),
        Error::UnexpectedReply
    );
    let mut c = client();
    c.begin(ClientInstant(0)).unwrap();
    c.queued(ClientInstant(1)).unwrap();
    let mut foreign = reply(1, 100);
    foreign[55] ^= 1;
    assert!(matches!(
        c.accept(&foreign, ClientInstant(2)),
        Err(Error::Wire(_))
    ));
    let mut c = client();
    c.begin(ClientInstant(0)).unwrap();
    c.queued(ClientInstant(1)).unwrap();
    c.accept(&reply(1, 100), ClientInstant(2)).unwrap();
    c.begin(ClientInstant(10)).unwrap();
    c.queued(ClientInstant(11)).unwrap();
    assert_eq!(
        c.accept(&reply(1, 100), ClientInstant(12)).unwrap_err(),
        Error::UnexpectedReply
    );
    assert_eq!(
        c.correlation(ClientInstant(13)).unwrap_err(),
        Error::Stopped
    );
}
#[test]
fn deadlines_are_exclusive_and_expiry_does_not_create_a_new_probe() {
    for queued in [false, true] {
        let mut c = client();
        c.begin(ClientInstant(100)).unwrap();
        if queued {
            c.queued(ClientInstant(200)).unwrap();
        }
        assert_eq!(c.begin(ClientInstant(300)), Err(Error::Busy));
        assert_eq!(c.tick(ClientInstant(1100)), Err(Error::Expired));
        assert_eq!(c.begin(ClientInstant(1101)), Err(Error::Stopped));
        assert_eq!(
            c.accept(&reply(1, 0), ClientInstant(1102)).unwrap_err(),
            Error::Stopped
        );
    }
}
#[test]
fn refresh_keeps_old_validity_and_requires_new_sequence() {
    let mut c = client();
    c.begin(ClientInstant(0)).unwrap();
    c.queued(ClientInstant(10)).unwrap();
    let first = c.accept(&reply(1, 100), ClientInstant(20)).unwrap();
    c.begin(ClientInstant(100)).unwrap();
    assert_eq!(
        c.correlation(ClientInstant(120))
            .unwrap()
            .unwrap()
            .valid_until_us(),
        first.valid_until_us()
    );
    c.queued(ClientInstant(140)).unwrap();
    let second = c.accept(&reply(2, 250), ClientInstant(160)).unwrap();
    assert_eq!(second.received_at_us(), 160);
    assert!(c.correlation(ClientInstant(10160)).unwrap().is_none());
}
#[test]
fn host_regression_local_regression_and_overflow_are_terminal() {
    let mut c = client();
    c.begin(ClientInstant(100)).unwrap();
    assert_eq!(c.tick(ClientInstant(99)), Err(Error::ClockRegression));
    let mut c = client();
    c.begin(ClientInstant(0)).unwrap();
    c.queued(ClientInstant(1)).unwrap();
    c.accept(&reply(1, 1000), ClientInstant(2)).unwrap();
    c.begin(ClientInstant(3)).unwrap();
    c.queued(ClientInstant(4)).unwrap();
    assert_eq!(
        c.accept(&reply(2, 999), ClientInstant(5)).unwrap_err(),
        Error::HostClockRegression
    );
    let mut c = client();
    assert_eq!(c.begin(ClientInstant(u64::MAX)), Err(Error::ClockOverflow));
}
#[test]
fn stop_never_preserves_a_pending_probe_or_a_usable_result() {
    let mut c = client();
    c.begin(ClientInstant(0)).unwrap();
    c.queued(ClientInstant(1)).unwrap();
    c.accept(&reply(1, 100), ClientInstant(2)).unwrap();
    c.stop();
    assert_eq!(c.correlation(ClientInstant(3)).unwrap_err(), Error::Stopped);
    assert_eq!(c.pending(ClientInstant(3)), Err(Error::Stopped));
}
