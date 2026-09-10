//! Real native input over ticket-attached streams, not preinstalled input routes.
//! Admission, initial control grant and presented-view evidence remain local fixtures.
use super::*;
use fr_transport::quic::{
    AttachedChannel, ChannelRequest, ChannelScope, ControlRoutes, MediaChannel,
};
use fr_wire::{
    attachment::{self, MediaRole, Ticket},
    decoder::Binding,
    negotiation::{Capability, ControlBinding, Offer, Role, Selection},
};
use frd::input_quic::NegotiatedInput;

pub(super) fn parent() -> ControlBinding {
    ControlBinding {
        id: 5,
        host_boot: HostBootId::from_raw(4),
        os_session: OsSessionId::from_raw(12),
        remote_session: credentials().session,
    }
}
fn bound(id: u32) -> Binding {
    Binding {
        parent: ControlBinding { id, ..parent() },
        display: 14,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
struct Attaching {
    client: QuicRecords,
    server: QuicRecords,
    cr: ControlRoutes,
    hr: ControlRoutes,
    selection: Selection,
}
impl Attaching {
    async fn new(cx: &Cx) -> Self {
        let (c, h) = network::native_pair(cx, "localhost", ALPN).await;
        let policy = Policy {
            critical_send_records: 1,
            ..Policy::default()
        };
        let (mut client, cr) = QuicRecords::bootstrap(c.unwrap(), cx, policy).unwrap();
        let (mut server, hr) = QuicRecords::bootstrap(h.unwrap(), cx, policy).unwrap();
        let cr = client.bind_control(cx, cr, 5, 4096, || true).unwrap();
        let hr = server.bind_control(cx, hr, 5, 4096, || true).unwrap();
        let selection = Offer {
            versions: vec![0],
            profile: 1,
            profile_version: 0,
            role: Role::RequestControl,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities: vec![
                Capability {
                    name: frd::input_quic::grant::CAPABILITY.into(),
                    version: 1,
                    required: true,
                },
                Capability {
                    name: attachment::INPUT_CAPABILITY.into(),
                    version: 1,
                    required: true,
                },
                Capability {
                    name: attachment::CAPABILITY.into(),
                    version: 1,
                    required: true,
                },
            ],
        }
        .select()
        .unwrap();
        Self {
            client,
            server,
            cr,
            hr,
            selection,
        }
    }
    async fn drive(&mut self, cx: &Cx) {
        let (a, b) = Box::pin(network::both(
            self.client.drive(cx, Duration::from_millis(1), || true),
            self.server.drive(cx, Duration::from_millis(1), || true),
        ))
        .await;
        a.unwrap();
        b.unwrap();
    }
    async fn attach(&mut self, cx: &Cx, role: MediaRole, id: u32) -> (MediaChannel, MediaChannel) {
        let mut h = self
            .server
            .offer_media_role(
                cx,
                ChannelScope {
                    control: self.hr,
                    parent: parent(),
                    selection: &self.selection,
                },
                ChannelRequest {
                    binding: bound(id),
                    ticket: Ticket(100 + u128::from(id)),
                    timeout: Duration::from_secs(2),
                },
                role,
                || true,
            )
            .unwrap();
        let end = network::clock(cx) + 1_500_000;
        let mut c = loop {
            assert!(network::clock(cx) < end);
            h.transmit(&mut self.server, cx, || true).unwrap();
            self.drive(cx).await;
            let mut bytes = None;
            self.client
                .receive_ready(
                    cx,
                    || true,
                    |r| r == Route::Stream(self.cr.inbound),
                    |_, b| {
                        assert!(bytes.is_none());
                        bytes = Some(b.to_vec());
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if let Some(bytes) = bytes {
                break self
                    .client
                    .accept_media_channel(
                        cx,
                        ChannelScope {
                            control: self.cr,
                            parent: parent(),
                            selection: &self.selection,
                        },
                        &bytes,
                        Duration::from_secs(2),
                        || true,
                    )
                    .unwrap();
            }
        };
        loop {
            assert!(network::clock(cx) < end);
            h.transmit(&mut self.server, cx, || true).unwrap();
            c.transmit(&mut self.client, cx, || true).unwrap();
            self.drive(cx).await;
            h.dispatch(&mut self.server, cx, || true).unwrap();
            c.dispatch(&mut self.client, cx, || true).unwrap();
            let a = h.finish(&mut self.server, cx, || true).unwrap();
            let b = c.finish(&mut self.client, cx, || true).unwrap();
            if a.is_some() && b.is_some() {
                break;
            }
        }
        (h, c)
    }
}
async fn joined(cx: &Cx) -> (Pair, NegotiatedInput, NegotiatedInput) {
    let (pair, host, viewer, _) = joined_selection(cx).await;
    (pair, host, viewer)
}
pub(super) async fn joined_selection(
    cx: &Cx,
) -> (Pair, NegotiatedInput, NegotiatedInput, Selection) {
    let mut l = Attaching::new(cx).await;
    let (hc, cc) = l.attach(cx, MediaRole::Configuration, 6).await;
    let (hi, ci) = l.attach(cx, MediaRole::Input, 7).await;
    let AttachedChannel {
        outbound: results,
        inbound,
        datagram,
        ..
    } = hi.completed_on(&l.server).unwrap();
    let (host, viewer) = (
        NegotiatedInput::new(&l.server, &l.selection, &hc, hi).unwrap(),
        NegotiatedInput::new(&l.client, &l.selection, &cc, ci).unwrap(),
    );
    let (actions, _, pointer) = viewer.viewer_routes(&l.client).unwrap();
    let routes = Routes::new(inbound, results, datagram).unwrap();
    let auxiliary = hc.completed_on(&l.server).unwrap().outbound;
    (
        Pair {
            client: l.client,
            server: l.server,
            actions,
            pointer,
            auxiliary,
            routes,
            control_routes: l.hr,
        },
        host,
        viewer,
        l.selection,
    )
}
fn send(f: &mut Fixture, viewer: &NegotiatedInput, bytes: &[u8]) {
    viewer
        .send(
            &f.cx,
            &mut f.pair.client,
            bytes,
            network::clock(&f.cx) + 1_000_000,
            || true,
        )
        .unwrap();
}
async fn until_receipts(f: &mut Fixture, viewer: &NegotiatedInput, n: usize) {
    let end = Instant::now() + Duration::from_secs(2);
    while f.receipts.len() < n {
        assert!(Instant::now() < end, "negotiated feedback did not arrive");
        f.turn_with(Some(viewer)).await;
    }
}
#[test]
fn negotiated_input_executes_native_drag_pointer_release_and_real_ticket_rollover() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (pair, host, viewer) = joined(&cx).await;
        let mut f = Fixture::from_pair(cx, pair, Some(host));
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            for (i, action) in [shift(KeyTransition::Press), button(true)]
                .into_iter()
                .enumerate()
            {
                let bytes = f.action(action);
                send(&mut f, &viewer, &bytes);
                until_receipts(&mut f, &viewer, i + 1).await;
                assert_eq!(f.receipts[i].outcome, InputOutcome::SubmittedToOs);
            }
            assert_eq!(f.observer.query_pointer().unwrap().1 & 0x0101, 257);
            let mut pointer = [0; MAX_INPUT_RECORD_BYTES];
            let n = f
                .client
                .pointer(
                    DesktopPoint { x: 80, y: 90 },
                    &mut pointer,
                    ClientInstant(network::clock(&f.cx)),
                )
                .unwrap();
            send(&mut f, &viewer, &pointer[..n.bytes]);
            until_receipts(&mut f, &viewer, 3).await;
            assert_eq!(
                f.observer.query_pointer().unwrap(),
                (DesktopPoint { x: 80, y: 90 }, 257)
            );
            f.input
                .renew_ticket(
                    &mut f.pair.server,
                    || true,
                    || Some(InputTicketId::from_raw(10)),
                )
                .unwrap();
            let end = Instant::now() + Duration::from_secs(1);
            while f.tickets.is_empty() {
                assert!(Instant::now() < end);
                f.turn_with(Some(&viewer)).await;
            }
            assert_eq!(f.tickets[0].credentials.ticket, InputTicketId::from_raw(10));
            for action in [button(false), shift(KeyTransition::Release)] {
                let bytes = f.action(action);
                let n = f.receipts.len() + 1;
                send(&mut f, &viewer, &bytes);
                until_receipts(&mut f, &viewer, n).await;
                assert_eq!(f.receipts[n - 1].outcome, InputOutcome::SubmittedToOs);
            }
            assert_eq!(f.client.pending_actions(), 0);
            assert_eq!(f.observer.query_pointer().unwrap().1 & 0x0101, 0);
            // The negotiated join did not steal the distinct renewal owner.
            let renewer = f
                .input
                .control_renewal(f.observation.clone(), &f.pair.server, f.pair.control_routes)
                .unwrap();
            drop(renewer);
            assert!(f.observation.check().is_ok());
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn negotiated_held_state_cleans_real_input_without_fabricating_action_receipts() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (pair, host, viewer) = joined(&cx).await;
        let mut f = Fixture::from_pair(cx, pair, Some(host));
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            for (i, action) in [shift(KeyTransition::Press), button(true)]
                .into_iter()
                .enumerate()
            {
                let b = f.action(action);
                send(&mut f, &viewer, &b);
                until_receipts(&mut f, &viewer, i + 1).await;
            }
            assert_eq!(f.observer.query_pointer().unwrap().1 & 0x0101, 257);
            let mut bytes = [0; fr_wire::held_state::HELD_STATE_BYTES];
            let held = f
                .client
                .reconcile_held(
                    fr_core::held_state::HeldState::default(),
                    &mut bytes,
                    ClientInstant(network::clock(&f.cx)),
                )
                .unwrap()
                .unwrap();
            send(&mut f, &viewer, &bytes[..held.bytes]);
            let end = Instant::now() + Duration::from_secs(1);
            while f.input.last_reconciliation().is_none() {
                assert!(Instant::now() < end);
                f.turn_with(Some(&viewer)).await;
            }
            assert_eq!(f.observer.query_pointer().unwrap().1 & 0x0101, 0);
            assert_eq!(f.receipts.len(), 2);
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn negotiated_viewer_refuses_foreign_connection_and_full_control_target_substitution() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (pair, host, viewer) = joined(&cx).await;
        let mut foreign = Attaching::new(&cx).await;
        let request = fr_wire::control::Request {
            parent: parent(),
            sequence: 0,
            target: fr_wire::control::Target {
                display_binding: 6,
                view: credentials().view,
                bounds: InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
                capabilities: fr_core::input_submission::Capabilities::default()
                    .with(fr_core::input_submission::Capability::Keys),
            },
        };
        viewer.check_request(&pair.client, request).unwrap();
        for i in 0..8 {
            let mut wrong = request;
            match i {
                0 => wrong.parent.host_boot = HostBootId::from_raw(55),
                1 => wrong.parent.os_session = OsSessionId::from_raw(55),
                2 => wrong.parent.remote_session = RemoteSessionId::from_raw(55),
                3 => wrong.target.display_binding = 55,
                4 => wrong.target.view.geometry = DisplayGeometryGeneration::from_raw(55),
                5 => wrong.target.view.viewport = ViewportMappingGeneration::from_raw(55),
                6 => wrong.target.view.recovery = RecoveryGeneration::from_raw(55),
                _ => wrong.target.view.configuration = CodecConfigurationGeneration::from_raw(55),
            }
            assert!(viewer.check_request(&pair.client, wrong).is_err());
        }
        let mut f = Fixture::from_pair(cx, pair, Some(host));
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let bytes = f.action(button(true));
            let before = foreign.client.usage();
            assert!(
                viewer
                    .send(
                        &f.cx,
                        &mut foreign.client,
                        &bytes,
                        network::clock(&f.cx) + 1_000_000,
                        || true
                    )
                    .is_err()
            );
            assert_eq!(before, foreign.client.usage());
            assert!(!foreign.client.is_closed());
            assert!(
                viewer
                    .receive_ready(
                        &f.cx,
                        &mut foreign.client,
                        || true,
                        |_| panic!("foreign feedback dispatched")
                    )
                    .is_err()
            );
            send(&mut f, &viewer, &bytes);
            until_receipts(&mut f, &viewer, 1).await;
            f.pair.server.close();
            assert!(f.input.service(&mut f.pair.server, || true).is_err());
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn negotiated_host_rejects_an_equal_numeric_but_independent_authority() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (pair, host, _) = joined(&cx).await;
        let display = Display::new();
        let mut observer = X11Pointer::open(&display.name).unwrap();
        let make = || {
            let now = host_now(&cx).unwrap();
            let c = credentials();
            let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
            a.mark_capabilities_checked().unwrap();
            a.authorize_observation(now).unwrap();
            a.mark_view_ready(now).unwrap();
            a.grant_lease(c.lease, now).unwrap();
            a.issue_input_ticket(c.lease, c.ticket, now).unwrap();
            frd::media::ObservationControl::new(cx.clone(), a).unwrap()
        };
        let real = make();
        let foreign = make();
        let seat = Seat::default();
        let session = real
            .input_session(credentials(), observer.bounds(), observer.capabilities())
            .unwrap();
        let (agent, driver) = start_x11(
            &seat,
            cx.clone(),
            session,
            AgentRoute::new(7, ProtocolLimits::ABSOLUTE),
            &display.name,
        )
        .unwrap();
        let before = pair.server.usage();
        assert!(
            host.into_host(cx.clone(), agent, &pair.server, &foreign)
                .is_err()
        );
        assert_eq!(before, pair.server.usage());
        assert!(!pair.server.is_closed());
        assert!(foreign.check().is_ok());
        assert!(driver.await.handoff_safe());
        assert!(!seat.is_occupied());
        assert_eq!(observer.query_pointer().unwrap().1 & 0x0101, 0);
    });
}

#[test]
fn negotiated_host_rejects_a_native_agent_for_a_different_view_generation() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let display = Display::new();
        let mut observer = X11Pointer::open(&display.name).unwrap();
        for dimension in 0..4 {
            let (pair, host, _) = joined(&cx).await;
            let now = host_now(&cx).unwrap();
            let mut c = credentials();
            match dimension {
                0 => c.view.geometry = DisplayGeometryGeneration::from_raw(1),
                1 => c.view.viewport = ViewportMappingGeneration::from_raw(1),
                2 => c.view.configuration = CodecConfigurationGeneration::from_raw(1),
                _ => c.view.recovery = RecoveryGeneration::from_raw(1),
            }
            let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
            a.mark_capabilities_checked().unwrap();
            a.authorize_observation(now).unwrap();
            a.mark_view_ready(now).unwrap();
            a.grant_lease(c.lease, now).unwrap();
            a.issue_input_ticket(c.lease, c.ticket, now).unwrap();
            let observation = frd::media::ObservationControl::new(cx.clone(), a).unwrap();
            let session = observation
                .input_session(c, observer.bounds(), observer.capabilities())
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
            let before = pair.server.usage();
            assert!(
                host.into_host(cx.clone(), agent, &pair.server, &observation)
                    .is_err()
            );
            assert_eq!(before, pair.server.usage());
            assert!(!pair.server.is_closed());
            assert!(driver.await.handoff_safe());
            assert!(!seat.is_occupied());
            assert_eq!(observer.query_pointer().unwrap().1 & 0x0101, 0);
        }
    });
}
