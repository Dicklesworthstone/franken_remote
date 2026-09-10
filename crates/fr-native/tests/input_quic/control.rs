//! Real network and native OS input; admission and renderer evidence are fixtures.
use super::*;
use fr_client::authority::ObservationResponder;
use fr_wire::authority::{self, Binding, Scope};
use frd::{
    input_quic::control::{ControlRenewal, Error as RenewalError, Event},
    media::renewal::ObservationRenewal,
};
struct Renewers {
    host: ControlRenewal,
    observation: ObservationRenewal,
    viewer_observation: ObservationResponder,
    next: u128,
    view_serial: u64,
    last_view: u64,
    responses: Vec<Vec<u8>>,
    controls: usize,
    observations: usize,
}
impl Renewers {
    fn new(f: &mut Fixture) -> Self {
        let routes = f.pair.control_routes;
        let host = f
            .input
            .control_renewal(f.observation.clone(), &f.pair.server, routes)
            .unwrap();
        let observation = ObservationRenewal::new(
            f.observation.clone(),
            &f.pair.server,
            routes,
            ProtocolLimits::ABSOLUTE,
        )
        .unwrap();
        let now = ClientInstant(network::clock(&f.cx));
        f.client.enable_control_renewal(11, now).unwrap();
        Self {
            host,
            observation,
            viewer_observation: ObservationResponder::new(
                Binding {
                    channel: 11,
                    session: credentials().session,
                },
                ProtocolLimits::ABSOLUTE,
                now,
            )
            .unwrap(),
            next: 100,
            view_serial: 1,
            last_view: now.0,
            responses: vec![],
            controls: 0,
            observations: 0,
        }
    }
    fn refresh_view(&mut self, f: &mut Fixture) {
        let now = ClientInstant(network::clock(&f.cx));
        if now.0 - self.last_view >= 400_000 {
            self.view_serial += 1;
            f.client
                .presented(
                    PresentedObservation {
                        session: credentials().session,
                        serial: self.view_serial,
                        view: credentials().view,
                        received_at: now,
                        source_age_upper_us: 0,
                    },
                    now,
                )
                .unwrap();
            self.last_view = now.0;
        }
    }
    async fn turn(&mut self, f: &mut Fixture, reply_control: bool) {
        self.refresh_view(f);
        self.observation
            .service(&mut f.pair.server, || {
                self.next += 1;
                Ok(self.next)
            })
            .unwrap();
        self.host
            .service(&mut f.pair.server, || {
                self.next += 1;
                Ok(self.next)
            })
            .unwrap();
        f.input.service(&mut f.pair.server, || true).unwrap();
        f.input
            .renew_ticket(
                &mut f.pair.server,
                || true,
                || {
                    self.next += 1;
                    Some(InputTicketId::from_raw(self.next))
                },
            )
            .unwrap();
        self.send_responses(f, reply_control);
        let (c, s) = Box::pin(network::both(
            f.pair
                .client
                .drive(&f.cx, Duration::from_millis(1), || true),
            self.host
                .drive(&mut f.pair.server, Duration::from_millis(1)),
        ))
        .await;
        c.unwrap();
        s.unwrap();
        self.observation
            .receive(&mut f.pair.server, |_, _| Ok(Disposition::Blocked))
            .unwrap();
        self.host
            .receive(&mut f.pair.server, |_, _| Ok(Disposition::Blocked))
            .unwrap();
        f.input
            .receive(
                &mut f.pair.server,
                || true,
                |_| false,
                |_, _| Ok(Disposition::Blocked),
            )
            .unwrap();
        self.receive_viewer(f, reply_control);
    }
    fn send_responses(&mut self, f: &mut Fixture, reply_control: bool) {
        let now = ClientInstant(network::clock(&f.cx));
        let route = Route::Stream(StreamRoute {
            outbound: true,
            ..f.pair.control_routes.inbound
        });
        let until = self.viewer_observation.response_deadline();
        if let Some(b) = self.viewer_observation.pending(now).unwrap() {
            let until = until.unwrap().0;
            match f.pair.client.send(&f.cx, route, b, until, || true) {
                Ok(()) => self.viewer_observation.sent(now).unwrap(),
                Err(quic::Error::Backpressure) => {}
                Err(e) => panic!("observation send: {e:?}"),
            }
        }
        if reply_control {
            let until = f.client.control_response_deadline();
            if let Some(b) = f.client.pending_control_response(now).unwrap() {
                let b = b.to_vec();
                match f
                    .pair
                    .client
                    .send(&f.cx, route, &b, until.unwrap().0, || true)
                {
                    Ok(()) => {
                        self.responses.push(b);
                        f.client.control_response_sent(now).unwrap();
                    }
                    Err(quic::Error::Backpressure) => {}
                    Err(e) => panic!("control send: {e:?}"),
                }
            }
        }
    }
    fn receive_viewer(&mut self, f: &mut Fixture, reply_control: bool) {
        let now = ClientInstant(network::clock(&f.cx));
        let control = Route::Stream(StreamRoute {
            outbound: false,
            ..f.pair.control_routes.outbound
        });
        f.pair
            .client
            .receive(
                &f.cx,
                || true,
                |route, b| {
                    if route == control {
                        let message = authority::decode(
                            b,
                            Binding {
                                channel: 11,
                                session: credentials().session,
                            },
                            &ProtocolLimits::ABSOLUTE,
                            InputDirection::HostToViewer,
                            InputDelivery::Reliable,
                        )
                        .unwrap();
                        match message.scope() {
                            Scope::Observation => match self.viewer_observation.accept(b, now) {
                                Ok(()) => self.observations += 1,
                                Err(fr_client::authority::Error::Backpressure) => {
                                    return Ok(Disposition::Blocked);
                                }
                                Err(e) => panic!("observation response {e:?}"),
                            },
                            Scope::Control(_) => {
                                self.controls += 1;
                                if reply_control {
                                    f.client.accept_control_challenge(b, now).unwrap();
                                }
                            }
                        }
                    } else if route
                        == Route::Stream(StreamRoute {
                            outbound: false,
                            ..f.pair.routes.results()
                        })
                    {
                        if b.get(6..8) == Some(&0x0017u16.to_be_bytes()) {
                            f.client.accept_ticket(b, f.clock, now).unwrap();
                            f.tickets.push(
                                fr_wire::input_ticket::decode(
                                    b,
                                    &ProtocolLimits::ABSOLUTE,
                                    7,
                                    InputDirection::HostToViewer,
                                    InputDelivery::Reliable,
                                )
                                .unwrap(),
                            );
                        } else if let ResultEvent::Completed(r) = f.client.result(b, now).unwrap() {
                            f.receipts.push(r);
                        }
                    }
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
    }
    async fn receipts(&mut self, f: &mut Fixture, count: usize) {
        let end = Instant::now() + Duration::from_secs(2);
        while f.receipts.len() < count {
            assert!(Instant::now() < end);
            self.turn(f, true).await;
        }
    }
}
#[test]
fn real_observation_control_and_tickets_keep_native_drag_alive_past_initial_lease() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new_options(cx, true).await;
        let mut r = Renewers::new(&mut f);
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            for action in [shift(KeyTransition::Press), button(true)] {
                let b = f.action(action);
                let n = f.receipts.len() + 1;
                f.send(&b, Route::Stream(f.pair.actions));
                r.receipts(&mut f, n).await;
            }
            let until = network::clock(&f.cx) + 3_250_000;
            while network::clock(&f.cx) < until {
                r.turn(&mut f, true).await;
            }
            assert!(!f.input.control().is_stopped());
            assert_eq!(f.observer.query_pointer().unwrap().1 & 0x101, 0x101);
            assert!(r.controls >= 4 && r.observations >= 4);
            assert!(f.tickets.len() >= 10);
            assert!(r.host.renewed_until().unwrap().as_micros() > until);
            for action in [button(false), shift(KeyTransition::Release)] {
                let b = f.action(action);
                let n = f.receipts.len() + 1;
                f.send(&b, Route::Stream(f.pair.actions));
                r.receipts(&mut f, n).await;
            }
            assert!(
                f.receipts
                    .iter()
                    .all(|x| x.outcome == InputOutcome::SubmittedToOs)
            );
            r.host.stop();
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn dropped_unpolled_control_drive_releases_real_drag_and_blocks_handoff_until_cleanup() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new_options(cx, true).await;
        let mut r = Renewers::new(&mut f);
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let b = f.action(button(true));
            f.send(&b, Route::Stream(f.pair.actions));
            r.receipts(&mut f, 1).await;
            drop(r.host.drive(&mut f.pair.server, Duration::from_millis(1)));
            assert!(f.input.control().is_stopped());
            assert!(f.pair.server.is_closed());
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn foreign_authority_and_duplicate_renewers_cannot_attach_to_native_owner() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new_options(cx.clone(), true).await;
        let other = Fixture::new_options(cx, true).await;
        assert!(matches!(
            f.input.control_renewal(
                other.observation.clone(),
                &f.pair.server,
                f.pair.control_routes
            ),
            Err(RenewalError::AuthorityMismatch)
        ));
        let mut renew = f
            .input
            .control_renewal(f.observation.clone(), &f.pair.server, f.pair.control_routes)
            .unwrap();
        assert!(matches!(
            f.input
                .control_renewal(f.observation.clone(), &f.pair.server, f.pair.control_routes),
            Err(RenewalError::AuthorityMismatch)
        ));
        renew.stop();
        other.input.control().stop(StopReason::LocalRevoke);
        let (a, b) = Box::pin(network::both(
            f.driver.take().unwrap(),
            other.driver.unwrap(),
        ))
        .await;
        assert!(a.handoff_safe() && b.handoff_safe());
    });
}
#[test]
fn exact_backpressure_cannot_slide_challenge_or_mint_replacement_nonce() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new_options(cx, true).await;
        let mut r = f
            .input
            .control_renewal(f.observation.clone(), &f.pair.server, f.pair.control_routes)
            .unwrap();
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            f.pair
                .server
                .send(
                    &f.cx,
                    Route::Stream(f.pair.auxiliary),
                    &filler(),
                    network::clock(&f.cx) + 2_000_000,
                    || true,
                )
                .unwrap();
            assert_eq!(
                r.service(&mut f.pair.server, || Ok(9)),
                Ok(Event::Backpressure)
            );
            asupersync::time::sleep(f.cx.now(), Duration::from_millis(1010)).await;
            assert_eq!(
                r.service(&mut f.pair.server, || panic!(
                    "must retain original challenge"
                )),
                Err(RenewalError::Expired)
            );
            assert!(f.input.control().is_stopped());
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn replayed_network_control_response_is_terminal_not_another_renewal() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new_options(cx, true).await;
        let mut r = Renewers::new(&mut f);
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let end = Instant::now() + Duration::from_secs(1);
            while r.host.renewed_until().is_none() {
                assert!(Instant::now() < end);
                r.turn(&mut f, true).await;
            }
            let until = r.host.renewed_until();
            let bytes = r.responses[0].clone();
            let route = Route::Stream(StreamRoute {
                outbound: true,
                ..f.pair.control_routes.inbound
            });
            // Allow the original response's retained native send storage to clear.
            for _ in 0..8 {
                r.turn(&mut f, true).await;
            }
            f.send(&bytes, route);
            let end = Instant::now() + Duration::from_secs(1);
            let error = loop {
                assert!(Instant::now() < end);
                f.io().await;
                if let Err(e) = r
                    .host
                    .receive(&mut f.pair.server, |_, _| Ok(Disposition::Blocked))
                {
                    break e;
                }
            };
            assert_eq!(error, RenewalError::UnexpectedResponse);
            assert_eq!(r.host.renewed_until(), until);
            assert!(f.input.control().is_stopped());
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

#[test]
fn observation_and_ticket_traffic_do_not_renew_unanswered_control() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new_options(cx, true).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let b = f.action(button(true));
            f.send(&b, Route::Stream(f.pair.actions));
            f.until_receipts(1).await;
            let mut r = Renewers::new(&mut f);
            let until = network::clock(&f.cx) + 2_700_000;
            while network::clock(&f.cx) < until {
                r.turn(&mut f, false).await;
            }
            assert!(r.observations >= 3);
            assert_eq!(r.controls, 1);
            assert!(f.tickets.len() >= 8);
            assert!(r.host.renewed_until().is_none());
            assert!(r.observation.renewed_until().unwrap().as_micros() > until + 1_000_000);
            assert!(!f.input.control().is_stopped());
            f.cleared().await;
            assert!(f.input.control().is_stopped());
            assert!(
                f.observation.check().is_ok(),
                "control loss is not observation renewal"
            );
            assert!(
                r.host
                    .service(&mut f.pair.server, || panic!(
                        "expired control must not reacquire"
                    ))
                    .is_err()
            );
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

#[test]
fn delayed_control_response_keeps_original_host_deadline() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new_options(cx, true).await;
        let mut r = Renewers::new(&mut f);
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            assert_eq!(
                r.host.service(&mut f.pair.server, || Ok(90)),
                Ok(Event::ChallengeQueued)
            );
            let mut captured = None;
            let end = Instant::now() + Duration::from_secs(1);
            while captured.is_none() {
                assert!(Instant::now() < end);
                f.io().await;
                f.pair
                    .client
                    .receive(
                        &f.cx,
                        || true,
                        |_, b| {
                            captured = Some(b.to_vec());
                            Ok(Disposition::Consumed)
                        },
                    )
                    .unwrap();
            }
            let bytes = captured.unwrap();
            let authority::Message::Challenge {
                deadline_micros, ..
            } = authority::decode(
                &bytes,
                Binding {
                    channel: 11,
                    session: credentials().session,
                },
                &ProtocolLimits::ABSOLUTE,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            )
            .unwrap()
            else {
                panic!("challenge required")
            };
            asupersync::time::sleep(f.cx.now(), Duration::from_millis(220)).await;
            let now = ClientInstant(network::clock(&f.cx));
            f.client.accept_control_challenge(&bytes, now).unwrap();
            r.send_responses(&mut f, true);
            let end = Instant::now() + Duration::from_secs(1);
            while r.host.renewed_until().is_none() {
                assert!(Instant::now() < end);
                f.io().await;
                r.host
                    .receive(&mut f.pair.server, |_, _| Ok(Disposition::Blocked))
                    .unwrap();
            }
            assert_eq!(r.host.renewed_until().unwrap().as_micros(), deadline_micros);
            assert!(deadline_micros - network::clock(&f.cx) < 2_850_000);
            r.host.stop();
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
