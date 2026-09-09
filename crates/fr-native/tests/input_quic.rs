#![cfg(all(target_os = "linux", feature = "linux-input-agent"))]
//! Real localhost UDP/TLS and real `XTest` effects. Grants and presentation
//! evidence are explicit local fixtures, not live tailnet admission or a GUI.
#[path = "../../fr-transport/tests/support/mod.rs"]
#[allow(dead_code)]
mod network;
use asupersync::{cx::Cx, types::Budget};
use fr_client::input::{
    Action, ClientInstant, InputClient, Policy as ClientPolicy, PresentedObservation, ResultEvent,
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::InputSession,
    limits::ProtocolLimits,
};
use fr_native::{input::X11Pointer, input_agent::start_x11};
use fr_transport::quic::{
    self, ALPN, DatagramRoute, Disposition, Messages, Policy, Priority, QuicRecords, Route,
    StreamRoute,
};
use fr_wire::{
    input::{InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES, encode_input},
    input_result::{INPUT_RESULT_BYTES, InputResult},
};
use frd::{
    input_agent::{AuthorityCommand, Driver, Route as AgentRoute, Seat},
    input_quic::{Error, Progress, QuicInput, Routes},
    input_watchdog::{StopReason, host_now},
};
use std::{
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Display {
    child: Child,
    name: String,
}
impl Display {
    fn new() -> Self {
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-nolisten",
                "tcp",
                "-noreset",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut number = String::new();
        BufReader::new(child.stdout.take().unwrap().take(16))
            .read_line(&mut number)
            .unwrap();
        Self {
            child,
            name: format!(":{}", number.trim().parse::<u32>().unwrap()),
        }
    }
}
impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn credentials() -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    }
}
struct Pair {
    client: QuicRecords,
    server: QuicRecords,
    actions: StreamRoute,
    pointer: DatagramRoute,
    auxiliary: StreamRoute,
    routes: Routes,
}
async fn pair(cx: &Cx) -> Pair {
    let (client, server) = network::native_pair(cx, "localhost", ALPN).await;
    let (mut client, mut server) = (client.unwrap(), server.unwrap());
    let actions = StreamRoute {
        stream: client.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 7,
        messages: Messages::InputActions,
        priority: Priority::Critical,
        outbound: true,
        maximum: MAX_INPUT_RECORD_BYTES,
    };
    let results = StreamRoute {
        stream: server.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 7,
        messages: Messages::Exact(0x48),
        priority: Priority::Critical,
        outbound: true,
        maximum: INPUT_RESULT_BYTES,
    };
    let auxiliary = StreamRoute {
        stream: server.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 9,
        messages: Messages::Exact(0x37),
        priority: Priority::Critical,
        outbound: true,
        maximum: 128,
    };
    let pointer = DatagramRoute {
        binding: 7,
        kind: 0x42,
        outbound: true,
    };
    let incoming = StreamRoute {
        outbound: false,
        ..actions
    };
    let incoming_pointer = DatagramRoute {
        outbound: false,
        ..pointer
    };
    let routes = Routes::new(incoming, results, Some(incoming_pointer)).unwrap();
    let policy = Policy {
        critical_send_records: 1,
        ..Policy::default()
    };
    Pair {
        client: QuicRecords::new(
            client,
            cx,
            &[
                actions,
                StreamRoute {
                    outbound: false,
                    ..results
                },
                StreamRoute {
                    outbound: false,
                    ..auxiliary
                },
            ],
            &[pointer],
            policy,
        )
        .unwrap(),
        server: QuicRecords::new(
            server,
            cx,
            &[incoming, results, auxiliary],
            &[incoming_pointer],
            policy,
        )
        .unwrap(),
        actions,
        pointer,
        auxiliary,
        routes,
    }
}
struct Fixture {
    cx: Cx,
    pair: Pair,
    input: QuicInput,
    client: InputClient,
    observer: X11Pointer,
    seat: Seat,
    driver: Option<Driver>,
    receipts: Vec<InputResult>,
    obsolete_pointers: usize,
    _display: Display,
}
impl Fixture {
    async fn new(cx: Cx) -> Self {
        let pair = pair(&cx).await;
        let display = Display::new();
        let observer = X11Pointer::open(&display.name).unwrap();
        let now = host_now(&cx).unwrap();
        let c = credentials();
        let mut authority = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
        authority.mark_capabilities_checked().unwrap();
        authority.authorize_observation(now).unwrap();
        authority.mark_view_ready(now).unwrap();
        authority.grant_lease(c.lease, now).unwrap();
        authority
            .issue_input_ticket(c.lease, c.ticket, now)
            .unwrap();
        let session = InputSession::new(
            authority,
            c,
            observer.bounds(),
            observer.capabilities(),
            now,
        )
        .unwrap();
        let seat = Seat::default();
        let (agent, driver) = start_x11(
            &seat,
            cx.clone(),
            session,
            AgentRoute::new(7, ProtocolLimits::ABSOLUTE),
            &display.name,
        )
        .unwrap();
        let input = QuicInput::new(cx.clone(), agent, &pair.server, pair.routes).unwrap();
        let time = ClientInstant(network::clock(&cx));
        let mut client = InputClient::new(
            c,
            7,
            observer.bounds(),
            observer.capabilities(),
            ProtocolLimits::ABSOLUTE,
            ClientPolicy {
                view_age_us: 1_500_000,
                receipt_timeout_us: 2_000_000,
            },
            time,
        )
        .unwrap();
        client.confirm_mapping(c.session, c.view, time).unwrap();
        client
            .presented(
                PresentedObservation {
                    session: c.session,
                    serial: 1,
                    view: c.view,
                    received_at: time,
                    source_age_upper_us: 0,
                },
                time,
            )
            .unwrap();
        Self {
            cx,
            pair,
            input,
            client,
            observer,
            seat,
            driver: Some(driver),
            receipts: vec![],
            obsolete_pointers: 0,
            _display: display,
        }
    }
    fn action(&mut self, action: Action<'_>) -> Vec<u8> {
        let mut bytes = vec![0; MAX_INPUT_RECORD_BYTES];
        let n = self
            .client
            .action(action, &mut bytes, ClientInstant(network::clock(&self.cx)))
            .unwrap();
        bytes.truncate(n.bytes);
        bytes
    }
    fn send(&mut self, bytes: &[u8], route: Route) {
        self.pair
            .client
            .send(
                &self.cx,
                route,
                bytes,
                network::clock(&self.cx) + 1_000_000,
                || true,
            )
            .unwrap();
    }
    async fn io(&mut self) {
        let (c, s) = Box::pin(network::both(
            self.pair
                .client
                .drive(&self.cx, Duration::from_millis(1), || true),
            self.input
                .drive(&mut self.pair.server, Duration::from_millis(1), || true),
        ))
        .await;
        c.unwrap();
        s.unwrap();
    }
    fn receive(&mut self) {
        self.input
            .receive(
                &mut self.pair.server,
                || true,
                |_| false,
                |_, _| panic!("unexpected host route"),
            )
            .unwrap();
        let now = ClientInstant(network::clock(&self.cx));
        let results = Route::Stream(StreamRoute {
            outbound: false,
            ..self.pair.routes.results()
        });
        self.pair
            .client
            .receive(
                &self.cx,
                || true,
                |r, bytes| {
                    if r == results {
                        match self.client.result(bytes, now).unwrap() {
                            ResultEvent::Completed(r) | ResultEvent::Pointer(r) => {
                                self.receipts.push(r);
                            }
                            ResultEvent::Duplicate(_) | ResultEvent::Unretained => {}
                        }
                    }
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
    }
    async fn turn(&mut self) -> Progress {
        let p = self.input.service(&mut self.pair.server, || true).unwrap();
        if p == Progress::ObsoletePointer {
            self.obsolete_pointers += 1;
        }
        self.io().await;
        self.receive();
        p
    }
    async fn until_receipts(&mut self, n: usize) {
        let end = Instant::now() + Duration::from_secs(2);
        while self.receipts.len() < n {
            assert!(Instant::now() < end, "receipt did not arrive");
            self.turn().await;
        }
    }
    async fn cleared(&mut self) {
        let end = Instant::now() + Duration::from_secs(2);
        while self.seat.is_occupied() {
            assert!(Instant::now() < end, "input cleanup did not complete");
            asupersync::time::sleep(self.cx.now(), Duration::from_millis(1)).await;
        }
        assert_eq!(self.observer.query_pointer().unwrap().1 & (1 | 256), 0);
    }
}
fn shift(transition: KeyTransition) -> Action<'static> {
    Action::Key {
        key: PhysicalKey::new(225).unwrap(),
        transition,
    }
}
fn button(pressed: bool) -> Action<'static> {
    Action::Button {
        button: PointerButton::Primary,
        pressed,
        position: DesktopPoint { x: 41, y: 53 },
    }
}

#[test]
fn real_client_quic_native_shift_drag_release_and_returned_receipts() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new(cx).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            for (i, action) in [
                shift(KeyTransition::Press),
                button(true),
                button(false),
                shift(KeyTransition::Release),
            ]
            .into_iter()
            .enumerate()
            {
                let bytes = f.action(action);
                f.send(&bytes, Route::Stream(f.pair.actions));
                f.until_receipts(i + 1).await;
                assert_eq!(f.receipts[i].sequence, i as u64);
                assert_eq!(f.receipts[i].outcome, InputOutcome::SubmittedToOs);
                if i == 1 {
                    assert_eq!(
                        f.observer.query_pointer().unwrap(),
                        (DesktopPoint { x: 41, y: 53 }, 257)
                    );
                }
            }
            assert_eq!(f.client.pending_actions(), 0);
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn ordered_release_wins_native_slot_over_an_already_buffered_pointer() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new(cx).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let b = f.action(button(true));
            f.send(&b, Route::Stream(f.pair.actions));
            f.until_receipts(1).await;
            let mut pointer = vec![0; MAX_INPUT_RECORD_BYTES];
            let n = f
                .client
                .pointer(
                    DesktopPoint { x: 150, y: 160 },
                    &mut pointer,
                    ClientInstant(network::clock(&f.cx)),
                )
                .unwrap();
            pointer.truncate(n.bytes);
            let release = f.action(button(false));
            f.send(&pointer, Route::Datagram(f.pair.pointer));
            f.send(&release, Route::Stream(f.pair.actions));
            // Receive neither input lane until BOTH records are in native QUIC.
            for _ in 0..8 {
                f.io().await;
            }
            assert_eq!(
                f.input
                    .receive(&mut f.pair.server, || true, |_| false, |_, _| panic!())
                    .unwrap(),
                1
            );
            f.until_receipts(2).await;
            assert_eq!(f.receipts[1].sequence, 1);
            assert_eq!(f.observer.query_pointer().unwrap().1 & 256, 0);
            let until = Instant::now() + Duration::from_secs(1);
            while f.obsolete_pointers == 0 && Instant::now() < until {
                f.turn().await;
            }
            assert_eq!(
                f.obsolete_pointers, 1,
                "old pointer must be consumed without rewinding release coordinates"
            );
            assert_eq!(
                f.observer.query_pointer().unwrap().0,
                DesktopPoint { x: 41, y: 53 }
            );
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
fn filler() -> Vec<u8> {
    let mut bytes = vec![0; 24];
    bytes[..4].copy_from_slice(b"FRD0");
    bytes[6..8].copy_from_slice(&0x37u16.to_be_bytes());
    bytes[16..20].copy_from_slice(&9u32.to_be_bytes());
    bytes
}
#[test]
fn receipt_backpressure_retains_identity_and_cannot_hold_a_revoked_drag() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new(cx).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let b = f.action(button(true));
            f.send(&b, Route::Stream(f.pair.actions));
            while !f.input.status().outstanding {
                f.io().await;
                f.receive();
            }
            // Occupy the one critical send slot before collecting the native
            // result. This is another route, not fake input/native submission.
            f.pair
                .server
                .send(
                    &f.cx,
                    Route::Stream(f.pair.auxiliary),
                    &filler(),
                    network::clock(&f.cx) + 1_000_000,
                    || true,
                )
                .unwrap();
            let end = Instant::now() + Duration::from_secs(1);
            while f.input.pending_receipt().is_none() {
                assert!(Instant::now() < end);
                f.input.service(&mut f.pair.server, || true).unwrap();
                asupersync::time::sleep(f.cx.now(), Duration::from_millis(1)).await;
            }
            let saved = f.input.pending_receipt().unwrap();
            assert!(!f.input.can_accept_input());
            for _ in 0..8 {
                assert_eq!(
                    f.input.service(&mut f.pair.server, || true).unwrap(),
                    Progress::ReceiptBackpressure
                );
            }
            assert_eq!(f.input.pending_receipt(), Some(saved));
            assert_eq!(f.observer.query_pointer().unwrap().1 & 256, 256);
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
            // Final receipts travel during closing drain, without re-authorizing
            // input or requiring the now-revoked input lease.
            f.until_receipts(1).await;
            assert_eq!(f.receipts[0], saved);
            assert!(!f.input.can_accept_input());
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn dropping_an_unpolled_network_drive_revokes_and_releases_native_input() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new(cx).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let b = f.action(button(true));
            f.send(&b, Route::Stream(f.pair.actions));
            f.until_receipts(1).await;
            drop(
                f.input
                    .drive(&mut f.pair.server, Duration::from_millis(1), || true),
            );
            assert!(f.pair.server.is_closed());
            assert!(f.input.control().is_stopped());
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn closed_or_replaced_connection_cannot_retain_control_or_receive_old_results() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new(cx.clone()).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let b = f.action(button(true));
            f.send(&b, Route::Stream(f.pair.actions));
            f.until_receipts(1).await;
            let mut replacement = pair(&cx).await;
            assert_eq!(
                f.input.service(&mut replacement.server, || true),
                Err(Error::WrongConnection)
            );
            assert!(!replacement.server.is_closed());
            f.cleared().await;
            assert_eq!(
                f.input.service(&mut f.pair.server, || true).unwrap(),
                Progress::Stopped
            );
            f.pair.server.close();
            assert_eq!(
                f.input.service(&mut f.pair.server, || true),
                Err(Error::Closed)
            );
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn failed_connection_admission_closes_before_native_effects() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new(cx).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            assert_eq!(
                f.input.service(&mut f.pair.server, || false),
                Err(Error::Transport(quic::Error::Unauthorized))
            );
            assert!(f.pair.server.is_closed());
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn local_authority_commands_share_native_ownership_without_becoming_network_input() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new(cx).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            f.input
                .authority(AuthorityCommand::ControlChallenge(42))
                .unwrap();
            assert!(!f.input.can_accept_input());
            let until = Instant::now() + Duration::from_secs(1);
            loop {
                assert!(Instant::now() < until);
                if let Progress::Authority(result) =
                    f.input.service(&mut f.pair.server, || true).unwrap()
                {
                    assert!(result.is_ok());
                    break;
                }
                f.io().await;
            }
            f.input
                .authority(AuthorityCommand::ControlResponse(42))
                .unwrap();
            loop {
                assert!(Instant::now() < until);
                if let Progress::Authority(result) =
                    f.input.service(&mut f.pair.server, || true).unwrap()
                {
                    assert!(result.is_ok());
                    break;
                }
                f.io().await;
            }
            assert!(f.input.can_accept_input());
            assert_eq!(f.pair.server.usage().critical_send_records, 0);
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn foreign_native_lease_is_refused_not_rewritten_into_a_successful_wire_receipt() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new(cx).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let mut c = credentials();
            c.lease = InputLeaseId::from_raw(99);
            let mut b = vec![0; MAX_INPUT_RECORD_BYTES];
            let n = encode_input(
                InputRequest {
                    credentials: c,
                    sequence: 0,
                    event: InputEvent::Button {
                        button: PointerButton::Primary,
                        pressed: true,
                        position: DesktopPoint { x: 41, y: 53 },
                        barrier: 0,
                    },
                },
                &mut b,
                &ProtocolLimits::ABSOLUTE,
                7,
                InputDirection::ViewerToHost,
                InputDelivery::Reliable,
            )
            .unwrap();
            b.truncate(n);
            f.send(&b, Route::Stream(f.pair.actions));
            let end = Instant::now() + Duration::from_secs(1);
            loop {
                assert!(Instant::now() < end);
                f.io().await;
                f.receive();
                match f.input.service(&mut f.pair.server, || true) {
                    Err(Error::ReceiptUnavailable(_)) => break,
                    Ok(_) => {}
                    other => panic!("unexpected result {other:?}"),
                }
            }
            assert!(f.pair.server.is_closed());
            assert!(f.input.pending_receipt().is_none());
            assert!(f.input.last_reply().is_some());
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

#[test]
fn malformed_action_payload_revokes_the_attachment_without_a_native_effect() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new(cx).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let mut bytes = f.action(button(true));
            // Correct FRD0 header and route, impossible empty Button payload.
            // This passes transport framing and reaches the native input codec.
            bytes.truncate(24);
            bytes[12..16].fill(0);
            f.send(&bytes, Route::Stream(f.pair.actions));
            let until = Instant::now() + Duration::from_secs(1);
            loop {
                assert!(Instant::now() < until);
                f.io().await;
                match f
                    .input
                    .receive(&mut f.pair.server, || true, |_| false, |_, _| panic!())
                {
                    Err(Error::Agent(frd::input_agent::Error::Wire(_))) => break,
                    Ok(_) => {}
                    other => panic!("unexpected refusal {other:?}"),
                }
            }
            assert!(f.pair.server.is_closed());
            assert!(f.input.last_reply().is_none());
            assert!(f.input.pending_receipt().is_none());
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

#[path = "input_quic/held.rs"]
mod held;
