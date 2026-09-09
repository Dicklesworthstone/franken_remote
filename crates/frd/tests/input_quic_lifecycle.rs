#![cfg(target_os = "linux")]
//! Actual QUIC UDP/TLS with explicitly blocked test-only native calls. The X11
//! integration is separately exercised by fr-native/tests/input_quic.rs.
#[path = "../../fr-transport/tests/support/mod.rs"]
#[allow(dead_code)]
mod network;
use asupersync::{bytes::Bytes, cx::Cx, net::quic_native::NativeQuicUdpConnection, types::Budget};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::*,
    limits::ProtocolLimits,
};
use fr_transport::quic::{ALPN, Messages, Policy, Priority, QuicRecords, StreamRoute};
use fr_wire::{
    input::{InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES, encode_input},
    input_result::INPUT_RESULT_BYTES,
};
use frd::{
    input_agent::{InputReply, Route, Seat},
    input_quic::QuicInput,
    input_quic::Routes,
    input_watchdog::{StopReason, host_now},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

#[derive(Default)]
struct Effects {
    entered: AtomicBool,
    pressed: AtomicUsize,
    released: AtomicUsize,
}
struct Gate {
    state: Arc<Effects>,
    resume: mpsc::Receiver<()>,
    in_submit: bool,
    waited: bool,
}
impl Gate {
    fn wait(&mut self) {
        if !self.waited {
            self.waited = true;
            self.state.entered.store(true, Ordering::Release);
            // Sender drop is the test failure fuse; no panic can strand a thread.
            let _ = self.resume.recv();
        }
    }
}
impl InputSink for Gate {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        if !self.in_submit {
            self.wait();
        }
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        if self.in_submit {
            self.wait();
        }
        match op {
            Operation::Key {
                transition: KeyTransition::Press,
                ..
            } => {
                self.state.pressed.fetch_add(1, Ordering::Relaxed);
            }
            Operation::Key {
                transition: KeyTransition::Release,
                ..
            } => {
                self.state.released.fetch_add(1, Ordering::Relaxed);
            }
            _ => panic!("unexpected native test operation"),
        }
        Submission::Submitted
    }
}
fn grant(cx: &Cx) -> (InputSession, InputCredentials) {
    let c = InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let now = host_now(cx).unwrap();
    let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(now).unwrap();
    a.mark_view_ready(now).unwrap();
    a.grant_lease(c.lease, now).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, now).unwrap();
    (
        InputSession::new(
            a,
            c,
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            Capabilities::default().with(Capability::Keys),
            now,
        )
        .unwrap(),
        c,
    )
}
async fn pump(
    cx: &Cx,
    peer: &mut NativeQuicUdpConnection,
    input: &QuicInput,
    wire: &mut QuicRecords,
) {
    let (a, b) = Box::pin(network::both(
        async {
            peer.flush(cx).await.unwrap();
            peer.drive_io_once(cx, Duration::from_millis(1))
                .await
                .unwrap();
        },
        input.drive(wire, Duration::from_millis(1), || true),
    ))
    .await;
    let () = a;
    b.unwrap();
}
async fn wire_pair(cx: &Cx) -> (NativeQuicUdpConnection, QuicRecords, Routes) {
    let (peer, host) = network::native_pair(cx, "localhost", ALPN).await;
    let (mut peer, mut host) = (peer.unwrap(), host.unwrap());
    let actions = StreamRoute {
        stream: peer.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 7,
        messages: Messages::InputActions,
        priority: Priority::Critical,
        outbound: false,
        maximum: MAX_INPUT_RECORD_BYTES,
    };
    let results = StreamRoute {
        stream: host.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 7,
        messages: Messages::Exact(0x48),
        priority: Priority::Critical,
        outbound: true,
        maximum: INPUT_RESULT_BYTES,
    };
    let wire = QuicRecords::new(host, cx, &[actions, results], &[], Policy::default()).unwrap();
    (peer, wire, Routes::new(actions, results, None).unwrap())
}
fn key_bytes(c: InputCredentials) -> Vec<u8> {
    let mut bytes = vec![0; MAX_INPUT_RECORD_BYTES];
    let n = encode_input(
        InputRequest {
            credentials: c,
            sequence: 0,
            event: InputEvent::Key {
                key: PhysicalKey::new(4).unwrap(),
                transition: KeyTransition::Press,
            },
        },
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}
fn terminated_while_blocked(reset: bool, in_submit: bool) {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (mut peer, mut wire, routes) = wire_pair(&cx).await;
        let actions = routes.actions();
        let (session, c) = grant(&cx);
        let (resume_tx, resume_rx) = mpsc::sync_channel(1);
        let effects = Arc::new(Effects::default());
        let native_effects = effects.clone();
        let seat = Seat::default();
        let (agent, driver) = seat
            .start(
                cx.clone(),
                session,
                Route::new(7, ProtocolLimits::ABSOLUTE),
                move || {
                    Ok(Gate {
                        state: native_effects,
                        resume: resume_rx,
                        in_submit,
                        waited: false,
                    })
                },
                |_| true,
            )
            .unwrap();
        let mut input = QuicInput::new(cx.clone(), agent, &wire, routes).unwrap();
        peer.connection_mut()
            .write_stream(&cx, actions.stream, Bytes::from(key_bytes(c)), false)
            .unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let until = Instant::now() + Duration::from_secs(1);
            while !effects.entered.load(Ordering::Acquire) {
                assert!(Instant::now() < until, "native call never entered");
                pump(&cx, &mut peer, &input, &mut wire).await;
                input
                    .receive(&mut wire, || true, |_| false, |_, _| panic!())
                    .unwrap();
            }
            // The peer closes the action stream while native work is blocked.
            // Do not release the native gate until the network path fences input.
            if reset {
                peer.connection_mut()
                    .reset_stream(&cx, actions.stream, 17)
                    .unwrap();
            } else {
                peer.connection_mut()
                    .write_stream(&cx, actions.stream, Bytes::new(), true)
                    .unwrap();
            }
            let until = Instant::now() + Duration::from_millis(500);
            while !input.control().is_stopped() && Instant::now() < until {
                pump(&cx, &mut peer, &input, &mut wire).await;
                input
                    .receive(&mut wire, || true, |_| false, |_, _| panic!())
                    .unwrap();
                input.service(&mut wire, || true).unwrap();
            }
            assert!(
                input.control().is_stopped(),
                "authenticated FIN/RESET must fence before native completion or lease expiry"
            );
            assert_eq!(
                input.control().reason(),
                Some(StopReason::ClientDisconnected)
            );
            assert!(
                seat.is_occupied(),
                "a fence is not confirmed native cleanup"
            );
            assert!(input.last_reply().is_none());
            assert_eq!(effects.pressed.load(Ordering::Relaxed), 0);
            resume_tx.send(()).unwrap();
            let until = Instant::now() + Duration::from_secs(1);
            while input.last_reply().is_none() || seat.is_occupied() {
                assert!(
                    Instant::now() < until,
                    "late receipt/cleanup did not finish"
                );
                input.service(&mut wire, || true).unwrap();
                asupersync::time::sleep(cx.now(), Duration::from_millis(1)).await;
            }
            let Some(InputReply::Record(r)) = input.last_reply() else {
                panic!("real receipt required")
            };
            assert_eq!(r.sequence, 0);
            if in_submit {
                assert_eq!(r.outcome, InputOutcome::SubmittedToOs);
                assert_eq!(r.submitted_operations, 1);
                assert_eq!(effects.pressed.load(Ordering::Relaxed), 1);
                assert_eq!(effects.released.load(Ordering::Relaxed), 1);
            } else {
                assert_eq!(r.outcome, InputOutcome::CancelledBeforeSubmission);
                assert_eq!(r.submitted_operations, 0);
                assert_eq!(effects.pressed.load(Ordering::Relaxed), 0);
                assert_eq!(effects.released.load(Ordering::Relaxed), 0);
            }
            assert!(!input.can_accept_input());
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn peer_fin_fences_blocked_preparation_before_the_native_call_returns() {
    terminated_while_blocked(false, false);
}
#[test]
fn peer_reset_fences_blocked_preparation_before_the_native_call_returns() {
    terminated_while_blocked(true, false);
}
#[test]
fn peer_fin_preserves_a_late_irreversible_effect_and_its_release() {
    terminated_while_blocked(false, true);
}
#[test]
fn peer_reset_preserves_a_late_irreversible_effect_and_its_release() {
    terminated_while_blocked(true, true);
}
