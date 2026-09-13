//! Actual production startup, ticketed configuration/input attachment and UDP/TLS.
//! Initial consent/grant, visibility and common-runtime clock are test fixtures.
//! The native executor uses a counted sink, not a claimed physical OS effect.
use super::*;
use crate::{
    input_agent::{self, Driver, Seat},
    session_startup::{ViewerSession, now, running::tests::pair_initialized, tests::support},
};
use asupersync::cx::Cx;
use fr_client::input::{
    Action, ClientInstant, InputClient, Policy, PresentedObservation, ResultEvent,
};
use fr_core::{
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::{
        Capabilities, Capability, InputSink, Operation as NativeOperation, PlatformError,
        Submission,
    },
    limits::ProtocolLimits,
};
use fr_media::freshness::{ClockCorrelation, ClockPolicy, ClockSample};
use fr_transport::quic::{self, ChannelRequest, MediaChannel};
use fr_wire::{
    attachment::{self, MediaRole, Ticket},
    decoder::Binding,
    negotiation::{Capability as WireCapability, ControlBinding},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

fn run<F, Fut>(f: F)
where
    F: FnOnce(Cx, Cx) -> Fut,
    Fut: Future<Output = ()>,
{
    crate::session_startup::running::tests::run(f);
}
#[allow(clippy::unnecessary_wraps)]
fn block(_: Route, _: &[u8]) -> Result<Disposition, ()> {
    Ok(Disposition::Blocked)
}
fn credentials() -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(13),
        lease: InputLeaseId::from_raw(19),
        ticket: InputTicketId::from_raw(23),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    }
}
fn bounds() -> InputBounds {
    InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap()
}
fn caps() -> Capabilities {
    Capabilities::default().with(Capability::Keys)
}
fn stamp(cx: &Cx) -> ClientInstant {
    ClientInstant(now(cx).unwrap())
}
fn nonce(n: &mut u128) -> Result<u128, ()> {
    *n = n.checked_add(1).ok_or(())?;
    Ok(*n)
}
fn ticket(n: &mut u128) -> Option<InputTicketId> {
    *n = n.checked_add(1)?;
    Some(InputTicketId::from_raw(*n))
}

pub(in crate::session_startup) async fn attach(
    host: &mut HostSession,
    viewer: &mut ViewerSession,
    c: &Cx,
    h: &Cx,
    role: MediaRole,
    id: u32,
) -> (MediaChannel, MediaChannel) {
    let binding = Binding {
        parent: ControlBinding {
            id,
            ..host.binding()
        },
        display: 9,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    };
    let observation = host.observation().unwrap();
    let mut hs = host
        .offer_media_role(
            ChannelRequest {
                binding,
                ticket: Ticket(u128::from(id) + 100),
                timeout: Duration::from_secs(2),
            },
            role,
        )
        .unwrap();
    let mut cs: Option<MediaChannel> = None;
    let mut n = 1000;
    let until = now(h).unwrap() + 1_000_000;
    loop {
        assert!(now(h).unwrap() < until, "attachment did not finish");
        hs.transmit(host.io().unwrap().0, h, || observation.check().is_ok())
            .unwrap();
        if let Some(cs) = &mut cs {
            cs.transmit(viewer.io().unwrap().0, c, || c.checkpoint().is_ok())
                .unwrap();
        }
        let mut offer = None;
        let (host_result, viewer_result) = Box::pin(support::both(
            host.drive(Duration::from_millis(1), || nonce(&mut n), block),
            viewer.drive(Duration::from_millis(1), |route, bytes| {
                if cs.is_none()
                    && matches!(route,Route::Stream(r) if r.binding==7)
                    && bytes[6..8] == (Kind::StreamBinding as u16).to_be_bytes()
                {
                    offer = Some(bytes.to_vec());
                    Ok(Disposition::Consumed)
                } else {
                    Ok(Disposition::Blocked)
                }
            }),
        ))
        .await;
        host_result.unwrap();
        viewer_result.unwrap();
        if let Some(offer) = offer {
            cs = Some(
                viewer
                    .accept_media_channel(&offer, Duration::from_secs(2))
                    .unwrap(),
            );
        }
        hs.dispatch(host.io().unwrap().0, h, || observation.check().is_ok())
            .unwrap();
        if let Some(client) = &mut cs {
            client
                .dispatch(viewer.io().unwrap().0, c, || c.checkpoint().is_ok())
                .unwrap();
            let host_completed = hs
                .finish(host.io().unwrap().0, h, || observation.check().is_ok())
                .unwrap();
            let viewer_completed = client
                .finish(viewer.io().unwrap().0, c, || c.checkpoint().is_ok())
                .unwrap();
            if host_completed.is_some() && viewer_completed.is_some() {
                return (hs, cs.take().unwrap());
            }
        }
    }
}

#[derive(Default)]
struct Effects {
    events: Vec<bool>,
    held: bool,
}
struct Sink {
    effects: Arc<Mutex<Effects>>,
    gate: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
}
impl InputSink for Sink {
    fn prepare(&mut self, _: NativeOperation) -> Result<(), PlatformError> {
        Ok(())
    }
    fn submit(&mut self, op: NativeOperation) -> Submission {
        if let Some((entered, release)) = self.gate.take() {
            entered.send(()).unwrap();
            release.recv().unwrap();
        }
        if let NativeOperation::Key { transition, .. } = op {
            let mut effects = self.effects.lock().unwrap();
            let pressed = transition != KeyTransition::Release;
            effects.events.push(pressed);
            effects.held = pressed;
        }
        Submission::Submitted
    }
}
struct Fixture {
    host: ControlledHost,
    viewer: View,
    driver: Driver,
    seat: Seat,
    effects: Arc<Mutex<Effects>>,
    initial_until: u64,
}
fn media_capabilities() -> Vec<WireCapability> {
    [
        fr_wire::decoder::CAPABILITY,
        attachment::INPUT_CAPABILITY,
        attachment::CAPABILITY,
        attachment::DELIVERY_CAPABILITY,
    ]
    .into_iter()
    .map(|name| WireCapability {
        name: name.into(),
        version: 1,
        required: true,
    })
    .collect()
}
async fn fixture(c: &Cx, h: &Cx, gate: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>) -> Fixture {
    Box::pin(fixture_with_clock(c, h, gate, false)).await
}
async fn fixture_with_clock(
    c: &Cx,
    h: &Cx,
    gate: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
    synchronized: bool,
) -> Fixture {
    let mut capabilities = media_capabilities();
    if synchronized {
        capabilities.push(WireCapability {
            name: fr_wire::clock::CAPABILITY.into(),
            version: fr_wire::clock::VERSION,
            required: true,
        });
        capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    }
    let mut initial_until = 0;
    let (mut host, mut viewer) = pair_initialized(c, h, capabilities, |host| {
        let a = host.authority.as_mut().unwrap();
        let t = HostInstant::from_micros(now(h).unwrap());
        a.mark_view_ready(t).unwrap();
        a.grant_lease(credentials().lease, t).unwrap();
        initial_until = a.control_deadline().unwrap().as_micros();
        a.issue_input_ticket(credentials().lease, credentials().ticket, t)
            .unwrap();
    })
    .await;
    if synchronized {
        host.enable_clock_sync().unwrap();
        viewer.enable_clock_sync(ClockPolicy::default()).unwrap();
    }
    let (hc, vc) = attach(&mut host, &mut viewer, c, h, MediaRole::Configuration, 8).await;
    let (hi, vi) = attach(&mut host, &mut viewer, c, h, MediaRole::Input, 10).await;
    let selection = host.selection().clone();
    let observation = host.observation().unwrap();
    let hn = input_quic::NegotiatedInput::new(host.io().unwrap().0, &selection, &hc, hi).unwrap();
    let vn = input_quic::NegotiatedInput::new(viewer.io().unwrap().0, &selection, &vc, vi).unwrap();
    let session = observation
        .input_session(credentials(), bounds(), caps())
        .unwrap();
    let seat = Seat::default();
    let effects = Arc::new(Mutex::new(Effects::default()));
    let sink_effects = effects.clone();
    let (agent, driver) = seat
        .start(
            h.clone(),
            session,
            input_agent::Route::new(10, ProtocolLimits::ABSOLUTE),
            move || {
                Ok(Sink {
                    effects: sink_effects,
                    gate,
                })
            },
            |s| !s.effects.lock().unwrap().held,
        )
        .unwrap();
    let input = hn
        .into_host(h.clone(), agent, host.io().unwrap().0, &observation)
        .unwrap();
    let host = host.into_controlled(input).unwrap();
    let t = stamp(c);
    let mut input = InputClient::new(
        credentials(),
        10,
        bounds(),
        caps(),
        ProtocolLimits::ABSOLUTE,
        Policy::default(),
        t,
    )
    .unwrap();
    input
        .confirm_mapping(credentials().session, credentials().view, t)
        .unwrap();
    input.enable_control_renewal(7, t).unwrap();
    let clock = ClockCorrelation::new(
        ClockSample {
            host_boot: HostBootId::from_raw(11),
            client_sent_us: t.0,
            host_sample_us: t.0,
            client_received_us: t.0,
        },
        ClockPolicy::default(),
    )
    .unwrap();
    Fixture {
        host,
        viewer: View {
            session: viewer,
            input,
            channel: vn,
            clock,
            serial: 0,
            tickets: 0,
            receipts: 0,
            pending: None,
        },
        driver,
        seat,
        effects,
        initial_until,
    }
}
struct View {
    session: ViewerSession,
    input: InputClient,
    channel: input_quic::NegotiatedInput,
    clock: ClockCorrelation,
    serial: u64,
    tickets: usize,
    receipts: usize,
    pending: Option<(Vec<u8>, u64)>,
}
impl View {
    fn visible(&mut self, c: &Cx) {
        self.serial += 1;
        let now = stamp(c);
        self.input
            .presented(
                PresentedObservation {
                    session: credentials().session,
                    serial: self.serial,
                    view: credentials().view,
                    received_at: now,
                    source_age_upper_us: 0,
                },
                now,
            )
            .unwrap();
    }
    fn queue_key(&mut self, c: &Cx, pressed: bool) {
        assert!(self.pending.is_none());
        let mut b = [0; 256];
        let encoded = self
            .input
            .action(
                Action::Key {
                    key: PhysicalKey::new(4).unwrap(),
                    transition: if pressed {
                        KeyTransition::Press
                    } else {
                        KeyTransition::Release
                    },
                },
                &mut b,
                stamp(c),
            )
            .unwrap();
        self.pending = Some((b[..encoded.bytes].to_vec(), now(c).unwrap() + 500_000));
    }
    fn send(&mut self, c: &Cx) {
        let (q, r) = self.session.io().unwrap();
        let until = self.input.control_response_deadline();
        if let Some(bytes) = self.input.pending_control_response(stamp(c)).unwrap() {
            match q.send(
                c,
                Route::Stream(r.outbound),
                bytes,
                until.unwrap().0,
                || c.checkpoint().is_ok(),
            ) {
                Ok(()) => self.input.control_response_sent(stamp(c)).unwrap(),
                Err(quic::Error::Backpressure) => {}
                Err(e) => panic!("control send {e:?}"),
            }
        }
        if let Some((bytes, until)) = &self.pending {
            match self
                .channel
                .send(c, q, bytes, *until, || c.checkpoint().is_ok())
            {
                Ok(()) => self.pending = None,
                Err(input_quic::Error::Transport(quic::Error::Backpressure)) => {}
                Err(e) => panic!("action send {e:?}"),
            }
        }
    }
    async fn drive(&mut self, c: &Cx) {
        self.visible(c);
        self.send(c);
        let Self {
            session,
            input,
            clock,
            tickets,
            receipts,
            ..
        } = self;
        session
            .drive(Duration::from_millis(2), |_, bytes| {
                match u16::from_be_bytes(bytes[6..8].try_into().unwrap()) {
                    k if k == Kind::Challenge as u16 => {
                        input.accept_control_challenge(bytes, stamp(c)).unwrap();
                    }
                    k if k == Kind::InputTicket as u16 => {
                        input.accept_ticket(bytes, *clock, stamp(c)).unwrap();
                        *tickets += 1;
                    }
                    k if k == Kind::InputResult as u16 => {
                        let event = input.result(bytes, stamp(c)).unwrap();
                        let ResultEvent::Completed(r) = event else {
                            panic!("unexpected receipt {event:?}")
                        };
                        assert_eq!(r.outcome, InputOutcome::SubmittedToOs);
                        *receipts += 1;
                    }
                    _ => return Ok(Disposition::Blocked),
                }
                Ok(Disposition::Consumed)
            })
            .await
            .unwrap();
    }
}
async fn turn(host: &mut ControlledHost, viewer: &mut View, c: &Cx, n: &mut u128, t: &mut u128) {
    let (a, ()) = Box::pin(support::both(
        host.drive(Duration::from_millis(2), || nonce(n), || ticket(t), block),
        viewer.drive(c),
    ))
    .await;
    a.unwrap();
}
#[test]
fn original_session_drives_native_receipts_and_both_renewals_beyond_initial_lease() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            mut viewer,
            driver,
            seat,
            effects,
            initial_until,
        } = Box::pin(fixture(&c, &h, None)).await;
        let connection = host.io().unwrap().0.binding();
        let mut n = 2000;
        let mut t = 5000;
        let ((), shutdown) = Box::pin(support::both(
            async {
                let mut pressed = false;
                let mut released = false;
                while now(&h).unwrap() < initial_until + 200_000 || viewer.receipts < 2 {
                    assert!(now(&h).unwrap() < initial_until + 900_000);
                    viewer.visible(&c);
                    if !pressed && viewer.tickets > 0 {
                        viewer.queue_key(&c, true);
                        pressed = true;
                    }
                    if !released
                        && pressed
                        && viewer.receipts == 1
                        && now(&h).unwrap() > initial_until
                    {
                        viewer.queue_key(&c, false);
                        released = true;
                    }
                    turn(&mut host, &mut viewer, &c, &mut n, &mut t).await;
                }
                assert!(host.control_renewed_until().unwrap().as_micros() > initial_until);
                assert!(host.observation_renewed_until().unwrap().as_micros() > initial_until);
                assert!(viewer.tickets >= 4);
                assert_eq!(effects.lock().unwrap().events, vec![true, false]);
                assert!(host.io().unwrap().0.is_bound_to(&connection));
                host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
    });
}

#[test]
fn pending_admission_refresh_keeps_control_tickets_and_input_results_progressing() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            mut viewer,
            driver,
            seat,
            effects,
            ..
        } = Box::pin(fixture(&c, &h, None)).await;
        viewer.visible(&c);
        viewer.queue_key(&c, true);
        let done = Arc::new(AtomicBool::new(false));
        let finished = done.clone();
        let (mut n, mut t) = (2000, 5000);
        let ((), shutdown) = Box::pin(support::both(
            async {
                let refresh = async {
                    asupersync::time::sleep(h.now(), Duration::from_millis(700)).await;
                    finished.store(true, Ordering::Release);
                    Ok(())
                };
                let observation = host.session.opened.control.clone();
                let mut fresh = || ticket(&mut t);
                let mut other = block;
                let mut services = InputServices {
                    input: &mut host.input,
                    renewal: &mut host.renewal,
                    observation: observation.clone(),
                    control: host.session.opened.routes,
                    ticket_turn: &mut host.ticket_turn,
                    submitted: &mut host.submitted,
                    ticket: &mut fresh,
                    other: &mut other,
                };
                let step = super::super::RefreshTurn {
                    cx: &h,
                    control: &observation,
                    until: now(&h).unwrap() + 1_100_000,
                    wait: Duration::from_millis(4),
                };
                let (a, ()) = Box::pin(support::both(
                    super::super::pump_refresh(
                        &mut host.session.renewal,
                        &mut host.session.opened.transport,
                        refresh,
                        step,
                        &mut || nonce(&mut n),
                        &mut services,
                    ),
                    async {
                        while !done.load(Ordering::Acquire) {
                            viewer.drive(&c).await;
                        }
                    },
                ))
                .await;
                a.unwrap();
                assert!(
                    viewer.tickets >= 2,
                    "native ticket issuance waited for LocalAPI completion"
                );
                assert_eq!(
                    viewer.receipts, 1,
                    "native results waited for LocalAPI completion"
                );
                assert!(host.control_renewed_until().is_some());
                assert!(host.observation_renewed_until().is_some());
                assert!(effects.lock().unwrap().held);
                host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
        assert_eq!(effects.lock().unwrap().events, vec![true, false]);
    });
}

#[test]
fn dropping_unpolled_controlled_turn_revokes_before_driver_cleanup() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            viewer: _viewer,
            driver,
            seat,
            ..
        } = Box::pin(fixture(&c, &h, None)).await;
        let control = host.control();
        let observation = host.session.observation().unwrap();
        let (mut n, mut t) = (2000, 5000);
        let future = host.drive(
            Duration::from_millis(10),
            || nonce(&mut n),
            || ticket(&mut t),
            block,
        );
        drop(future);
        assert!(control.is_stopped());
        assert!(observation.check().is_err());
        assert!(host.session.opened.transport.is_closed());
        let shutdown = driver.await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
        assert!(host.collect_after_close().unwrap().is_none());
    });
}

#[test]
fn invalid_wait_fences_original_authorities_without_issuing_credentials() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            viewer: _viewer,
            driver,
            seat,
            ..
        } = Box::pin(fixture(&c, &h, None)).await;
        let (mut n, mut t) = (2000, 5000);
        let result = host
            .drive(
                Duration::from_secs(2),
                || nonce(&mut n),
                || ticket(&mut t),
                block,
            )
            .await;
        assert!(matches!(result, Err(Error::InvalidConfiguration)));
        assert_eq!((n, t), (2000, 5000));
        assert!(host.control().is_stopped());
        let shutdown = driver.await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
    });
}

#[test]
fn held_native_call_cannot_block_renewal_and_its_late_result_survives_close() {
    run(|c, h| async move {
        let (entered_tx, entered) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let Fixture {
            mut host,
            mut viewer,
            driver,
            seat,
            effects,
            initial_until,
        } = Box::pin(fixture(&c, &h, Some((entered_tx, gate)))).await;
        let (mut n, mut t) = (2000, 5000);
        let mut command = false;
        let mut entered_call = false;
        let ((), shutdown) = Box::pin(support::both(
            async {
                while now(&h).unwrap() < initial_until + 100_000 {
                    viewer.visible(&c);
                    // Enter close enough to original lease expiry to cross it without
                    // exceeding the independent two-second client receipt deadline.
                    if viewer.tickets > 0 && !command && now(&h).unwrap() >= initial_until - 400_000
                    {
                        viewer.queue_key(&c, true);
                        command = true;
                    }
                    turn(&mut host, &mut viewer, &c, &mut n, &mut t).await;
                    entered_call |= entered.try_recv().is_ok();
                }
                assert!(entered_call);
                assert_eq!(effects.lock().unwrap().events, [] as [bool; 0]);
                assert!(host.control_renewed_until().unwrap().as_micros() > initial_until);
                assert!(host.observation_renewed_until().unwrap().as_micros() > initial_until);
                assert_eq!(viewer.receipts, 0);
                assert!(host.native_status().outstanding);
                host.close();
                assert!(host.control().is_stopped());
                assert!(seat.is_occupied());
                assert!(host.collect_after_close().unwrap().is_none());
                release.send(()).unwrap();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
        let reply = host
            .collect_after_close()
            .unwrap()
            .expect("actual late native result lost");
        let crate::input_agent::InputReply::Record(result) = reply else {
            panic!("actual input result required {reply:?}")
        };
        assert_eq!(result.outcome, InputOutcome::SubmittedToOs);
        assert_eq!(effects.lock().unwrap().events, vec![true, false]);
    });
}

#[test]
fn nonce_failure_closes_input_instead_of_leaving_an_unserviced_live_lease() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            viewer: _viewer,
            driver,
            seat,
            ..
        } = Box::pin(fixture(&c, &h, None)).await;
        let mut t = 5000;
        let result = host
            .drive(
                Duration::from_millis(2),
                || Err(()),
                || ticket(&mut t),
                block,
            )
            .await;
        assert!(matches!(
            result,
            Err(Error::ControlRenewal(
                crate::input_quic::control::Error::NonceUnavailable
            ))
        ));
        assert!(host.control().is_stopped());
        assert!(host.session.opened.transport.is_closed());
        assert_eq!(t, 5000);
        assert!(driver.await.handoff_safe());
        assert!(!seat.is_occupied());
    });
}

#[test]
#[allow(clippy::too_many_lines)]
fn buffered_ordered_input_cannot_take_a_due_tickets_fair_turn() {
    run(|c, h| async move {
        let (entered_tx, entered) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let Fixture {
            mut host,
            mut viewer,
            driver,
            seat,
            effects,
            ..
        } = Box::pin(fixture(&c, &h, Some((entered_tx, gate)))).await;
        let (mut n, mut t) = (2000, 5000);
        let ((), shutdown) = Box::pin(support::both(
            async {
                while viewer.tickets == 0 {
                    turn(&mut host, &mut viewer, &c, &mut n, &mut t).await;
                }
                viewer.visible(&c);
                viewer.queue_key(&c, true);
                let until = now(&h).unwrap() + 800_000;
                while entered.try_recv().is_err() {
                    assert!(now(&h).unwrap() < until, "native press never entered");
                    turn(&mut host, &mut viewer, &c, &mut n, &mut t).await;
                }
                assert!(host.ticket_turn);
                viewer.visible(&c);
                viewer.queue_key(&c, false);
                let route = Route::Stream(host.input.routes().actions());
                let mut buffered = false;
                let renewal_due = now(&h).unwrap() + 250_000;
                // Retain the actual second input record while the native call owns
                // the mailbox. Advance the real clock, not the scheduler deadline.
                while !buffered || now(&h).unwrap() < renewal_due {
                    assert!(now(&h).unwrap() < until, "ordered record not retained");
                    let (a, ()) = Box::pin(support::both(
                        host.session
                            .opened
                            .transport
                            .drive(&h, Duration::from_millis(2), || true),
                        viewer.drive(&c),
                    ))
                    .await;
                    a.unwrap();
                    host.session
                        .opened
                        .transport
                        .receive_ready(
                            &h,
                            || true,
                            |r| r == route,
                            |_, _| {
                                buffered = true;
                                Ok(Disposition::Blocked)
                            },
                        )
                        .unwrap();
                }
                release.send(()).unwrap();
                // Collect and transmit the original receipt, without consuming the
                // buffered release or granting another mailbox turn prematurely.
                while !host.input.can_accept_input() {
                    assert!(now(&h).unwrap() < until, "receipt did not complete");
                    host.input
                        .service(&mut host.session.opened.transport, || true)
                        .unwrap();
                    let (a, ()) = Box::pin(support::both(
                        host.session
                            .opened
                            .transport
                            .drive(&h, Duration::from_millis(2), || true),
                        viewer.drive(&c),
                    ))
                    .await;
                    a.unwrap();
                }
                assert!(host.ticket_turn);
                assert_eq!(effects.lock().unwrap().events, vec![true]);
                let mut issued = 0;
                let mut fresh = || {
                    issued += 1;
                    ticket(&mut t)
                };
                let mut other = block;
                let mut services = InputServices {
                    input: &mut host.input,
                    renewal: &mut host.renewal,
                    observation: host.session.opened.control.clone(),
                    control: host.session.opened.routes,
                    ticket_turn: &mut host.ticket_turn,
                    submitted: &mut host.submitted,
                    ticket: &mut fresh,
                    other: &mut other,
                };
                services
                    .maintain(&mut host.session.opened.transport, &mut || nonce(&mut n))
                    .unwrap();
                assert_eq!(issued, 1, "buffered input stole the due ticket's turn");
                assert_eq!(effects.lock().unwrap().events, vec![true]);
                while viewer.receipts < 2 {
                    assert!(now(&h).unwrap() < until);
                    turn(&mut host, &mut viewer, &c, &mut n, &mut t).await;
                }
                assert_eq!(effects.lock().unwrap().events, vec![true, false]);
                host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
    });
}

#[test]
fn revoke_during_refresh_poll_blocks_shared_udp_before_the_next_service_turn() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            viewer: _viewer,
            driver,
            seat,
            ..
        } = Box::pin(fixture(&c, &h, None)).await;
        let control = host.control();
        let observation = host.session.opened.control.clone();
        let refresh = async {
            // This poll occurs AFTER the maintenance hooks enqueue critical
            // records, but BEFORE the retained shared UDP future is polled.
            control.stop(StopReason::LocalRevoke);
            Ok(())
        };
        let (mut n, mut t) = (2000, 5000);
        let mut fresh = || ticket(&mut t);
        let mut other = block;
        let mut services = InputServices {
            input: &mut host.input,
            renewal: &mut host.renewal,
            observation: observation.clone(),
            control: host.session.opened.routes,
            ticket_turn: &mut host.ticket_turn,
            submitted: &mut host.submitted,
            ticket: &mut fresh,
            other: &mut other,
        };
        let result = super::super::pump_refresh(
            &mut host.session.renewal,
            &mut host.session.opened.transport,
            refresh,
            super::super::RefreshTurn {
                cx: &h,
                control: &observation,
                until: now(&h).unwrap() + 500_000,
                wait: Duration::from_millis(10),
            },
            &mut || nonce(&mut n),
            &mut services,
        )
        .await;
        assert!(matches!(
            result,
            Err(Error::Renewal(crate::media::renewal::Error::Transport(
                quic::Error::Unauthorized
            )))
        ));
        assert!(control.is_stopped());
        assert!(host.session.opened.transport.is_closed());
        assert!(observation.check().is_err());
        assert!(driver.await.handoff_safe());
        assert!(!seat.is_occupied());
    });
}

#[test]
fn streaming_codec_stall_does_not_hold_input_results_tickets_or_local_cleanup() {
    crate::session_startup::running::streaming::tests::run(|c, h| async move {
        let Fixture {
            mut host,
            mut viewer,
            driver,
            seat,
            effects,
            ..
        } = Box::pin(fixture(&c, &h, None)).await;
        let stream = Box::pin(
            crate::session_startup::running::streaming::tests::source_for_controlled(
                &mut host.session,
                &mut viewer.session,
                &c,
                &h,
            ),
        )
        .await;
        let stop = host.control();
        let mut host = host.into_streaming(stream).unwrap();
        let (mut n, mut t) = (2000, 5000);
        let ((), shutdown) = Box::pin(support::both(
            async {
                viewer.visible(&c);
                viewer.queue_key(&c, true);
                let until = now(&c).unwrap() + 700_000;
                let (result, ()) = Box::pin(support::both(
                    host.serve(|| nonce(&mut n), || ticket(&mut t), block),
                    async {
                        while now(&c).unwrap() < until {
                            viewer.drive(&c).await;
                        }
                        assert_eq!(viewer.receipts, 1, "codec stalled native receipts");
                        assert!(viewer.tickets >= 2, "codec stalled ticket renewal");
                        assert!(effects.lock().unwrap().held);
                        stop.stop(StopReason::LocalRevoke);
                    },
                ))
                .await;
                assert!(result.is_err());
                assert_eq!(
                    host.statistics().encoded_updates,
                    0,
                    "stalled capture produced fabricated output"
                );
                host.reap_media(
                    &c,
                    crate::worker::Deadline::after(&c, Duration::from_secs(1)).unwrap(),
                )
                .await
                .unwrap();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
        assert_eq!(effects.lock().unwrap().events, vec![true, false]);
    });
}

mod input_wake;
