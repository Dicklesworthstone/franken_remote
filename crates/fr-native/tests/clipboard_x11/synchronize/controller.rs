//! Real X11 desktops with an actual accepted viewer grant. The record handoff
//! and protocol clocks are deterministic fixtures, NOT a live-tailnet claim.
use super::*;
use fr_client::{
    clipboard::{ControllerSynchronizer, Error as ControllerError, NativeError},
    control_grant::RequestControl,
    input::{ClientInstant, InputClient, Policy, PresentedObservation, StopReason},
};
use fr_media::freshness::{ClockCorrelation, ClockPolicy, ClockSample};
use fr_wire::{
    control::{self, Granted, Request, Target},
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
};

const CLIENT_ORIGIN: u64 = 7_000_000_000_000;
fn client(elapsed: u64) -> ClientInstant {
    ClientInstant(CLIENT_ORIGIN + 100 + elapsed)
}
fn controller() -> InputClient {
    let g = Granted {
        request: Request {
            parent: ControlBinding {
                id: 7,
                host_boot: HostBootId::from_raw(4),
                os_session: OsSessionId::from_raw(5),
                remote_session: RemoteSessionId::from_raw(1),
            },
            sequence: 1,
            target: Target {
                display_binding: 8,
                view: InputView {
                    geometry: DisplayGeometryGeneration::INITIAL,
                    viewport: ViewportMappingGeneration::INITIAL,
                    configuration: CodecConfigurationGeneration::INITIAL,
                    recovery: RecoveryGeneration::INITIAL,
                },
                bounds: InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
                capabilities: Capabilities::default()
                    .with(fr_core::input_submission::Capability::Keys),
            },
        },
        input_channel: 9,
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        issued_at_us: 0,
        lease_until_us: 3_000_000,
        ticket_until_us: 1_000_000,
        first_action: 0,
        first_pointer: 0,
    };
    let mut request = RequestControl::new(
        g.request,
        g.input_channel,
        ProtocolLimits::ABSOLUTE,
        ClientInstant(CLIENT_ORIGIN),
    )
    .unwrap();
    request.sent(ClientInstant(CLIENT_ORIGIN)).unwrap();
    let mut bytes = [0; control::GRANTED_BYTES];
    control::encode_granted(
        g,
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    let clock = ClockCorrelation::new(
        ClockSample {
            host_boot: g.request.parent.host_boot,
            client_sent_us: CLIENT_ORIGIN,
            client_received_us: client(0).0,
            host_sample_us: 0,
        },
        ClockPolicy {
            drift_ppm: 0,
            ..ClockPolicy::default()
        },
    )
    .unwrap();
    let (_, mut input) = request
        .accept(
            &bytes,
            clock,
            Policy {
                view_age_us: 1_500_000,
                ..Policy::default()
            },
            client(0),
        )
        .unwrap();
    input
        .confirm_mapping(g.credentials().session, g.credentials().view, client(0))
        .unwrap();
    input
        .presented(
            PresentedObservation {
                session: g.credentials().session,
                view: g.credentials().view,
                serial: 1,
                received_at: client(0),
                source_age_upper_us: 100,
            },
            client(0),
        )
        .unwrap();
    input
}

struct ControllerPair {
    host: ClipboardSynchronizer,
    viewer: ControllerSynchronizer<X11Clipboard>,
    host_app: X11Clipboard,
    viewer_app: X11Clipboard,
    host_input: InputOwner,
    viewer_input: Option<InputClient>,
    to_viewer: Record,
    to_host: Record,
    scratch: Vec<u8>,
    ids: [u128; 2],
    receipts: [usize; 2],
    desktops: [Desktop; 2],
}
impl ControllerPair {
    fn new() -> Self {
        let desktops = [Desktop::new(), Desktop::new()];
        let host_input = owner();
        let mut viewer_input = controller();
        let limits = ProtocolLimits::with_overrides(fr_core::limits::LimitOverrides {
            max_control_message_bytes: Some(2048),
            ..fr_core::limits::LimitOverrides::default()
        })
        .unwrap();
        let parent = ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(4),
            os_session: OsSessionId::from_raw(5),
            remote_session: RemoteSessionId::from_raw(1),
        };
        let outgoing = Context {
            scope: Binding {
                session: parent.remote_session,
                lease: InputLeaseId::from_raw(2),
            },
            channel: 77,
            sender: Role::Controller,
            lane: Lane::Clipboard,
        };
        let viewer = viewer_input
            .attach_clipboard_lane(parent, outgoing, limits, true, client(0))
            .unwrap()
            .into_native(open(&desktops[1].display));
        let mut pair = Self {
            host: ClipboardSynchronizer::new(
                ChannelSession::new(
                    &host_input,
                    Context {
                        sender: Role::Host,
                        ..outgoing
                    },
                    limits,
                    true,
                    at(0),
                )
                .unwrap(),
                open(&desktops[0].display),
            ),
            viewer,
            host_app: open(&desktops[0].display),
            viewer_app: open(&desktops[1].display),
            host_input,
            viewer_input: Some(viewer_input),
            to_viewer: Record::default(),
            to_host: Record::default(),
            scratch: vec![0; 65_536],
            ids: [0, 0],
            receipts: [0, 0],
            desktops,
        };
        pair.step();
        pair
    }
    fn step(&mut self) {
        self.host_app.pump().unwrap();
        self.viewer_app.pump().unwrap();
        self.host
            .poll(
                &mut self.scratch,
                &mut self.to_viewer,
                || at(100),
                || {
                    self.ids[0] += 1;
                    Ok(self.ids[0])
                },
            )
            .unwrap();
        self.viewer
            .poll(
                &mut self.scratch,
                &mut self.to_host,
                || client(0),
                || {
                    self.ids[1] += 1;
                    Ok(1000 + self.ids[1])
                },
            )
            .unwrap();
        if let Some(bytes) = self.to_viewer.bytes.as_deref() {
            assert!(bytes.len() <= 2048);
            let received = self.viewer.receive(bytes, || client(0)).unwrap();
            if received != Received::Deferred {
                self.to_viewer.clear();
                match received {
                    Received::Consumed(Some(receipt)) => {
                        assert_eq!(receipt.publication, Publication::SubmittedToOs);
                        self.receipts[1] += 1;
                    }
                    Received::Consumed(None) => {}
                    other => panic!("unexpected clipboard receipt: {other:?}"),
                }
            }
        }
        if let Some(bytes) = self.to_host.bytes.as_deref() {
            assert!(bytes.len() <= 2048);
            let received = self.host.receive(bytes, || at(100)).unwrap();
            if received != Received::Deferred {
                self.to_host.clear();
                match received {
                    Received::Consumed(Some(receipt)) => {
                        assert_eq!(receipt.publication, Publication::SubmittedToOs);
                        self.receipts[0] += 1;
                    }
                    Received::Consumed(None) => {}
                    other => panic!("unexpected clipboard receipt: {other:?}"),
                }
            }
        }
    }
    fn until(&mut self, condition: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition(self) {
            self.step();
            assert!(Instant::now() < deadline, "native viewer transfer stalled");
            std::thread::sleep(Duration::from_micros(100));
        }
    }
    fn text(&mut self, index: usize) -> String {
        let mut reader = open(&self.desktops[index].display);
        reader.begin_read().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            self.step();
            if let Some(text) = reader.poll_read().unwrap() {
                return text.as_str().to_owned();
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_micros(100));
        }
    }
}

#[test]
fn accepted_controller_automatically_copies_both_ways_between_real_x11_desktops() {
    let _guard = SERIAL.lock().unwrap();
    if display().is_none() {
        return;
    }
    let mut pair = ControllerPair::new();
    for (index, text) in [
        String::new(),
        "viewer 🦀 café\0\n".to_owned(),
        "🦀".repeat(262_144),
    ]
    .iter()
    .enumerate()
    {
        publish(&mut pair.host_app, text, (index + 1) as u64);
        pair.until(|p| p.receipts[1] == index + 1);
        assert_eq!(pair.text(1), *text);
        publish(&mut pair.viewer_app, text, (index + 1) as u64);
        pair.until(|p| p.receipts[0] == index + 1);
        assert_eq!(pair.text(0), *text);
    }
    let sent = [pair.to_host.accepted, pair.to_viewer.accepted];
    for _ in 0..100 {
        pair.step();
    }
    assert_eq!(pair.ids, [3, 3]);
    assert_eq!([pair.to_host.accepted, pair.to_viewer.accepted], sent);
    assert_eq!(pair.viewer.retained_channel_bytes(), 0);
    assert!(pair.host_input.monitor().deadline(at(100)).is_ok());
    pair.viewer_input.as_mut().unwrap().tick(client(0)).unwrap();
}

#[test]
fn stopped_dropped_or_stale_viewer_cancels_real_incr_and_preserves_native_copy() {
    let _guard = SERIAL.lock().unwrap();
    if display().is_none() {
        return;
    }
    for mode in 0..3 {
        let mut pair = ControllerPair::new();
        publish(&mut pair.viewer_app, &"x".repeat(1_048_576), 1);
        pair.until(|p| p.viewer_app.active_readers() == 1);
        if mode == 0 {
            pair.viewer_input
                .as_mut()
                .unwrap()
                .stop(StopReason::FocusLost);
        }
        if mode == 1 {
            drop(pair.viewer_input.take());
        }
        let result = pair.viewer.poll(
            &mut pair.scratch,
            &mut pair.to_host,
            || client(if mode == 2 { 1_500_000 } else { 0 }),
            || panic!("no retry"),
        );
        assert!(matches!(
            result,
            Err(NativeError::Authority(
                ControllerError::Stopped | ControllerError::Expired
            ))
        ));
        assert!(pair.viewer.is_closed());
        assert_eq!(pair.viewer.retained_channel_bytes(), 0);
        assert_eq!(pair.to_host.accepted, 0);
        assert_eq!(pair.receipts[0], 0);
        assert_eq!(pair.viewer_app.current_origin().unwrap(), Some(stamp(1)));
        assert!(pair.host_input.monitor().deadline(at(100)).is_ok());
        assert!(
            pair.viewer
                .poll(
                    &mut pair.scratch,
                    &mut pair.to_host,
                    || client(0),
                    || panic!("no retry")
                )
                .is_err()
        );
    }
}

#[test]
fn viewer_switch_cycles_cancel_without_replay_and_allow_a_genuine_new_copy() {
    let _guard = SERIAL.lock().unwrap();
    if display().is_none() {
        return;
    }
    for local in [true, false] {
        let mut pair = ControllerPair::new();
        publish(&mut pair.viewer_app, &"x".repeat(1_048_576), 1);
        pair.until(|p| p.viewer_app.active_readers() == 1);
        let switch = if local {
            pair.viewer.local_switch()
        } else {
            pair.viewer.peer_switch()
        };
        switch.set_enabled(false);
        switch.set_enabled(true);
        for _ in 0..30 {
            pair.step();
        }
        assert_eq!(pair.receipts[0], 0);
        assert_eq!(pair.ids[1], 1);
        assert_eq!(pair.viewer.retained_channel_bytes(), 0);
        pair.viewer_input.as_mut().unwrap().tick(client(0)).unwrap();
        publish(&mut pair.viewer_app, "new user copy", 2);
        pair.until(|p| p.receipts[0] == 1);
        assert_eq!(pair.text(0), "new user copy");
        assert_eq!(pair.ids[1], 2);
    }
}
