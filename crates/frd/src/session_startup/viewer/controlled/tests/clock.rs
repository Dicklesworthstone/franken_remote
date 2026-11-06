//! Actual ordered UDP/TLS delivery and the retained clock/control owners.
//! Authority prefixes and invalid replies are explicit peer fixtures; they do
//! not issue host authority, submit native input, or claim physical visibility.
use super::*;
use crate::media::clock::{Error as ClockError, Event as ClockEvent};
use fr_wire::{
    authority::{self, Binding, Message, Scope},
    clock as wire_clock,
    input::{InputDelivery, InputDirection},
};

async fn raw_turn(state: &mut Fixture, c: &Cx, h: &Cx, until: u64) {
    assert!(now(h).unwrap() < until, "ordered peer staging expired");
    let (host, viewer) = Box::pin(support::both(
        state
            .host
            .io()
            .unwrap()
            .0
            .drive(h, Duration::from_millis(1), || true),
        state
            .viewer
            .session
            .transport
            .drive(c, Duration::from_millis(1), || true),
    ))
    .await;
    host.unwrap();
    viewer.unwrap();
}

async fn drain_host(state: &mut Fixture, c: &Cx, h: &Cx, until: u64) {
    while state.host.io().unwrap().0.usage().retained_send_records != 0 {
        raw_turn(state, c, h, until).await;
    }
}

async fn enqueue(state: &mut Fixture, c: &Cx, h: &Cx, bytes: &[u8], until: u64) {
    loop {
        assert!(now(h).unwrap() < until, "original peer admission expired");
        let (q, routes) = state.host.io().unwrap();
        match q.send(h, Route::Stream(routes.outbound), bytes, until, || true) {
            Ok(()) => break,
            Err(quic::Error::Backpressure) => raw_turn(state, c, h, until).await,
            other => panic!("control admission: {other:?}"),
        }
    }
    // This fixture has ONE critical send slot. Await its actual acknowledgement
    // without running the viewer dispatcher: the prefix must remain unread.
    drain_host(state, c, h, until).await;
}

fn challenge(state: &Fixture, h: &Cx) -> Vec<u8> {
    let parent = state.viewer.session.opened.binding;
    let mut bytes = [0; authority::MAX_AUTHORITY_BYTES];
    let n = authority::encode(
        Message::Challenge {
            scope: Scope::Observation,
            nonce: crate::session_startup::tests::unique_nonce(),
            // Later than the fixture's previous three-second host challenges.
            // Its response is never applied to the host's authority owner.
            deadline_micros: now(h).unwrap() + 4_000_000,
        },
        Binding {
            channel: parent.id,
            session: parent.remote_session,
        },
        &state.viewer.input.protocol_limits(),
        &mut bytes,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes[..n].to_vec()
}

async fn begin_refresh(state: &mut Fixture, c: &Cx, h: &Cx) {
    let until = now(c).unwrap() + 2_000_000;
    let mut counter = 10_000;
    let mut stamp = 20_000;
    loop {
        assert!(now(c).unwrap() < until, "original clock did not refresh");
        match state
            .viewer
            .clock
            .service(&mut state.viewer.session.transport)
            .unwrap()
        {
            ClockEvent::ProbeQueued | ClockEvent::AwaitingReply => return,
            ClockEvent::Idle | ClockEvent::Backpressure => {}
            other @ ClockEvent::ReplyQueued => panic!("viewer clock event: {other:?}"),
        }
        // Keep real authority, tickets, view progress and the independently
        // polled Driver alive until the original one-second refresh is due.
        let (host, viewer) = turn(state, c, h, &mut counter, &mut stamp).await;
        host.unwrap();
        viewer.unwrap();
    }
}

async fn receive_probe(state: &mut Fixture, c: &Cx, h: &Cx, until: u64) {
    let mut counter = 50_000;
    let mut stamp = 60_000;
    loop {
        assert!(now(h).unwrap() < until, "original probe did not arrive");
        let q = state.host.io().unwrap().0;
        if state
            .host_clock
            .as_mut()
            .unwrap()
            .receive(q, block)
            .unwrap()
            != 0
        {
            return;
        }
        // A prior genuine renewal response may precede the probe. Its real host
        // owner consumes it; the viewer only pumps transport, never a reply.
        let (host, viewer) = Box::pin(support::both(
            state.host.drive(
                Duration::from_millis(1),
                || nonce(&mut counter),
                || {
                    stamp += 1;
                    Some(InputTicketId::from_raw(stamp))
                },
                block,
            ),
            state
                .viewer
                .session
                .transport
                .drive(c, Duration::from_millis(1), || true),
        ))
        .await;
        host.unwrap();
        viewer.unwrap();
    }
}

fn preliminary_receive_blocks(state: &mut Fixture) {
    let inbound = Route::Stream(state.viewer.session.routes.inbound);
    let mut saw_challenge = false;
    let consumed = state
        .viewer
        .clock
        .receive(&mut state.viewer.session.transport, |route, bytes| {
            if route == inbound && bytes.get(6..8) == Some(&(Kind::Challenge as u16).to_be_bytes())
            {
                saw_challenge = true;
            }
            Ok(Disposition::Blocked)
        })
        .unwrap();
    assert!(saw_challenge, "the clock reply was not behind a challenge");
    assert_eq!(consumed, 0);
}

#[allow(clippy::unnecessary_wraps)] // Exact transport callback signature.
fn no_clock_escape(_: Route, bytes: &[u8]) -> Result<Disposition, ()> {
    let kind = bytes.get(6..8);
    assert!(
        kind != Some(&(Kind::ClockReply as u16).to_be_bytes())
            && kind != Some(&(Kind::ClockProbe as u16).to_be_bytes()),
        "an owned clock record escaped into the application callback"
    );
    Ok(Disposition::Blocked)
}

#[test]
fn an_ordered_authority_challenge_does_not_hide_the_original_clock_refresh() {
    run(|c, h| async move {
        let mut state = Box::pin(fixture(&c, &h)).await;
        let driver = state.driver.take().unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                begin_refresh(&mut state, &c, &h).await;
                let until = now(&h).unwrap() + 750_000;
                receive_probe(&mut state, &c, &h, until).await;
                let binding = state.viewer.input.binding();
                let connection = state.viewer.session.transport.binding();
                let observation_until = state.host.observation_renewed_until();
                let control_until = state.host.control_renewed_until();
                let before = state.viewer.clock_at;
                let bytes = challenge(&state, &h);
                enqueue(&mut state, &c, &h, &bytes, until).await;
                loop {
                    let q = state.host.io().unwrap().0;
                    match state.host_clock.as_mut().unwrap().service(q).unwrap() {
                        ClockEvent::ReplyQueued => break,
                        ClockEvent::Backpressure => raw_turn(&mut state, &c, &h, until).await,
                        other => panic!("host clock event: {other:?}"),
                    }
                }
                drain_host(&mut state, &c, &h, until).await;
                preliminary_receive_blocks(&mut state);
                loop {
                    assert!(now(&h).unwrap() < until, "clock dispatch remained blocked");
                    state
                        .viewer
                        .drive(
                            Duration::ZERO,
                            |_| panic!("unexpected input receipt"),
                            no_clock_escape,
                        )
                        .await
                        .unwrap();
                    if state.viewer.clock_at > before {
                        break;
                    }
                    raw_turn(&mut state, &c, &h, until).await;
                }
                let sample = state
                    .viewer
                    .clock
                    .correlation(&mut state.viewer.session.transport)
                    .unwrap()
                    .unwrap();
                assert_eq!(state.viewer.input.clock_correlation(), sample);
                assert_eq!(state.viewer.clock_at, sample.received_at_us());
                assert_eq!(state.viewer.input.binding(), binding);
                assert!(state.viewer.session.transport.is_bound_to(&connection));
                assert!(state.viewer.input.stopped().is_none());
                assert!(!state.viewer.control().is_stopped());
                assert!(!state.host.control().is_stopped());
                assert_eq!(state.host.observation_renewed_until(), observation_until);
                assert_eq!(state.host.control_renewed_until(), control_until);
                assert_eq!(state.effects.lock().unwrap().operations, [] as [Op; 0]);
                state.viewer.close();
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!state.seat.is_occupied());
    });
}

fn refuse_reply(change: fn(&mut [u8]), expected: ClockError) {
    run(|c, h| async move {
        let mut state = Box::pin(fixture(&c, &h)).await;
        let driver = state.driver.take().unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                let until = now(&h).unwrap() + 750_000;
                let prefix = challenge(&state, &h);
                enqueue(&mut state, &c, &h, &prefix, until).await;
                let mut bytes = [0; wire_clock::REPLY_BYTES];
                // Sequence one was consumed by the fixture's actual exchange.
                // These are hostile peer records, not fabricated measurements.
                wire_clock::encode(
                    wire_clock::Message::Reply {
                        sequence: 1,
                        host_sample_us: now(&h).unwrap(),
                    },
                    state.viewer.session.opened.binding,
                    &state.viewer.input.protocol_limits(),
                    &mut bytes,
                    InputDirection::HostToViewer,
                    InputDelivery::Reliable,
                )
                .unwrap();
                change(&mut bytes);
                enqueue(&mut state, &c, &h, &bytes, until).await;
                preliminary_receive_blocks(&mut state);
                let error = loop {
                    assert!(
                        now(&h).unwrap() < until,
                        "invalid clock record stayed blocked"
                    );
                    match state
                        .viewer
                        .drive(Duration::ZERO, |_| {}, no_clock_escape)
                        .await
                    {
                        Err(error) => break error,
                        Ok(()) => raw_turn(&mut state, &c, &h, until).await,
                    }
                };
                assert_eq!(error, Error::Clock(expected));
                assert!(state.viewer.is_closed());
                assert!(state.viewer.control().is_stopped());
                assert!(!state.viewer.pending_send());
                assert_eq!(state.effects.lock().unwrap().operations, [] as [Op; 0]);
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!state.seat.is_occupied());
    });
}

#[test]
fn a_malformed_clock_reply_behind_a_challenge_stops_its_original_owner() {
    refuse_reply(
        |bytes| bytes[72..80].fill(0),
        ClockError::Wire(WireError::InvalidValue),
    );
}

#[test]
fn a_foreign_clock_binding_behind_a_challenge_stops_its_original_owner() {
    refuse_reply(
        |bytes| bytes[39] ^= 1,
        ClockError::Wire(WireError::InvalidBinding),
    );
}

#[test]
fn a_replayed_clock_sequence_behind_a_challenge_stops_its_original_owner() {
    refuse_reply(
        |_| {},
        ClockError::Client(fr_client::clock::Error::UnexpectedReply),
    );
}
