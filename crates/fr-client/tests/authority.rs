use fr_client::{
    authority::{Error, ObservationResponder},
    input::ClientInstant,
};
use fr_core::{
    ids::{InputLeaseId, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_wire::{
    authority::{self, Binding, Message, Scope},
    input::{InputDelivery as T, InputDirection as D},
};
const L: ProtocolLimits = ProtocolLimits::ABSOLUTE;
fn binding() -> Binding {
    Binding {
        channel: 7,
        session: RemoteSessionId::from_raw(0x0012_3456),
    }
}
fn challenge(nonce: u128, deadline: u64, scope: Scope) -> Vec<u8> {
    let mut bytes = [0; 82];
    let n = authority::encode(
        Message::Challenge {
            scope,
            nonce,
            deadline_micros: deadline,
        },
        binding(),
        &L,
        &mut bytes,
        D::HostToViewer,
        T::Reliable,
    )
    .unwrap();
    bytes[..n].to_vec()
}
fn responder(now: u64) -> ObservationResponder {
    ObservationResponder::new(binding(), L, ClientInstant(now)).unwrap()
}
#[test]
fn opaque_host_clock_and_exact_reply_do_not_need_symmetric_clocks() {
    for now in [0, 9_000_000_000] {
        let mut r = responder(now);
        r.accept(&challenge(9, 1, Scope::Observation), ClientInstant(now))
            .unwrap();
        assert_eq!(
            authority::decode(
                r.pending(ClientInstant(now)).unwrap().unwrap(),
                binding(),
                &L,
                D::ViewerToHost,
                T::Reliable
            )
            .unwrap(),
            Message::Response {
                scope: Scope::Observation,
                nonce: 9
            }
        );
        r.sent(ClientInstant(now + 1)).unwrap();
        assert!(r.pending(ClientInstant(now + 1)).unwrap().is_none());
    }
}
#[test]
fn backpressure_keeps_exact_bytes_and_never_slides_deadline() {
    let mut r = responder(10);
    r.accept(&challenge(9, 99, Scope::Observation), ClientInstant(10))
        .unwrap();
    let old = r.pending(ClientInstant(11)).unwrap().unwrap().to_vec();
    assert_eq!(
        r.accept(&challenge(10, 100, Scope::Observation), ClientInstant(12)),
        Err(Error::Backpressure)
    );
    assert_eq!(r.pending(ClientInstant(1_000_009)).unwrap().unwrap(), old);
    assert_eq!(r.pending(ClientInstant(1_000_010)), Err(Error::Expired));
    assert!(r.stopped());
    assert_eq!(
        r.accept(
            &challenge(10, 100, Scope::Observation),
            ClientInstant(1_000_011)
        ),
        Err(Error::Stopped)
    );
}
#[test]
fn duplicates_reordered_challenges_wrong_scope_and_stop_cannot_renew() {
    for bytes in [
        challenge(9, 101, Scope::Observation),
        challenge(10, 100, Scope::Observation),
        challenge(10, 99, Scope::Observation),
    ] {
        let mut r = responder(0);
        r.accept(&challenge(9, 100, Scope::Observation), ClientInstant(0))
            .unwrap();
        r.sent(ClientInstant(1)).unwrap();
        assert_eq!(
            r.accept(&bytes, ClientInstant(2)),
            Err(Error::StaleChallenge)
        );
    }
    let mut r = responder(0);
    assert_eq!(
        r.accept(
            &challenge(9, 100, Scope::Control(InputLeaseId::from_raw(4))),
            ClientInstant(0)
        ),
        Err(Error::WrongScope)
    );
    let mut r = responder(0);
    r.stop();
    assert_eq!(
        r.accept(&challenge(9, 100, Scope::Observation), ClientInstant(0)),
        Err(Error::Stopped)
    );
}
#[test]
fn successive_fresh_challenges_can_renew_without_retaining_history_or_payloads() {
    let mut r = responder(0);
    for n in 1_u64..100 {
        r.accept(
            &challenge(u128::from(n), n, Scope::Observation),
            ClientInstant(n),
        )
        .unwrap();
        r.sent(ClientInstant(n)).unwrap();
    }
    assert!(!r.stopped());
    assert!(std::mem::size_of::<ObservationResponder>() < 512);
}
#[test]
fn clock_faults_and_malformed_bodies_are_terminal() {
    let mut r = responder(2);
    assert_eq!(r.tick(ClientInstant(1)), Err(Error::Clock));
    let mut r = responder(u64::MAX);
    assert_eq!(
        r.accept(
            &challenge(1, 1, Scope::Observation),
            ClientInstant(u64::MAX)
        ),
        Err(Error::Clock)
    );
    let mut r = responder(0);
    assert!(matches!(
        r.accept(&[0; 66], ClientInstant(0)),
        Err(Error::Wire(_))
    ));
    assert!(r.stopped());
    let mut r = responder(0);
    assert_eq!(r.sent(ClientInstant(0)), Err(Error::StaleChallenge));
}
