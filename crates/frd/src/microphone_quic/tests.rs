//! Real UDP/TLS and attachment ownership. Input establishment is an explicit
//! policy fixture, not native input or tailnet qualification. Real device/codec
//! execution is separately gated below and never replaced with a fake worker.
use super::*;
use crate::{
    input_agent::{Driver, Route as InputRoute, Seat},
    input_quic::Routes,
    input_watchdog::StopReason,
    test_support::{authority, transport_support},
};
use fr_core::{
    audio::{AudioChannels, AudioDirection, AudioGeneration},
    input_submission::{InputSink, Operation, PlatformError, Submission},
    limits::ProtocolLimits,
};
use fr_wire::{attachment::MediaRole, audio::AudioConfiguration, negotiation::Role};
use std::{path::Path, time::Duration};
use transport_support::{clock, runtime};

#[path = "tests/network.rs"]
mod attachment;

struct PolicyOnlyInput;
impl InputSink for PolicyOnlyInput {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        panic!("microphone tests must not submit native input")
    }
    fn submit(&mut self, _: Operation) -> Submission {
        panic!("microphone tests must not submit native input")
    }
}
struct Fixture {
    cx: Cx,
    link: attachment::Link,
    input: QuicInput,
    _driver: Driver,
    input_pair: (quic::MediaChannel, quic::MediaChannel),
    lane: Option<AudioUpChannel>,
    peer: AudioUpChannel,
    peer_control: quic::StreamRoute,
}
fn absent_launch() -> native::Launch {
    native::Launch::new(
        Path::new("/no-installed-microphone-device"),
        Path::new("/no-installed-microphone-codec"),
        Path::new("/no-installed-microphone-server/pipewire-0"),
    )
    .unwrap()
}
impl Fixture {
    async fn new() -> Self {
        let cx = Cx::current().unwrap();
        let mut link = attachment::Link::new(&cx).await;
        let input = link.attach(&cx, 9, MediaRole::Input).await;
        let routes = input.0.completed_on(&link.h).unwrap();
        let routes = Routes::new(routes.inbound, routes.outbound, routes.datagram).unwrap();
        let grant = authority::grant(&cx);
        let seat = Seat::default();
        let (agent, driver) = seat
            .start(
                cx.clone(),
                authority::host(grant),
                InputRoute::new(9, ProtocolLimits::ABSOLUTE),
                || Ok(PolicyOnlyInput),
                |_| true,
            )
            .unwrap();
        let original = QuicInput::new(cx.clone(), agent, &link.h, routes).unwrap();
        let (h, c) = link.attach(&cx, 10, MediaRole::AudioUp).await;
        let peer_control = c.completed_on(&link.c).unwrap().outbound;
        let lane = AudioUpChannel::new(&link.h, h).unwrap();
        let peer = AudioUpChannel::new(&link.c, c).unwrap();
        // The final attachment reply is real retained transport work. Consume
        // its native empty-send witness before later ACK-capacity assertions.
        let until = clock(&cx) + 1_000_000;
        while !lane.control_send_drained(&link.h).unwrap()
            || !peer.control_send_drained(&link.c).unwrap()
        {
            assert!(clock(&cx) < until, "attachment queues did not drain");
            link.drive(&cx).await;
        }
        Self {
            cx,
            link,
            input: original,
            _driver: driver,
            input_pair: input,
            lane: Some(lane),
            peer,
            peer_control,
        }
    }
    fn host(&mut self, approved: bool, launch: native::Launch) -> Result<Host, AttachError> {
        Host::new(
            &mut self.link.h,
            self.lane.take().unwrap(),
            &self.input,
            &self.link.selection,
            approved,
            launch,
        )
    }
    fn send(&mut self, bytes: &[u8]) {
        assert_eq!(
            self.peer
                .send_control_prepared(
                    &mut self.link.c,
                    &self.cx,
                    bytes,
                    clock(&self.cx) + 1_000_000,
                    || true,
                    || Ok::<_, ()>(()),
                )
                .unwrap(),
            SendAdmission::Accepted
        );
    }
    async fn drive(&mut self, host: &Host) {
        let (a, b) = Box::pin(transport_support::both(
            self.link
                .h
                .drive(&self.cx, Duration::from_micros(200), || host.permits_io()),
            self.link
                .c
                .drive(&self.cx, Duration::from_micros(200), || true),
        ))
        .await;
        a.unwrap();
        b.unwrap();
    }
}
fn configuration(generation: u64, direction: AudioDirection) -> Vec<u8> {
    let mut bytes = vec![0; audio::AUDIO_CONFIGURATION_RECORD_BYTES];
    audio::encode_configuration(
        &AudioConfiguration {
            direction,
            generation: AudioGeneration::from_raw(generation),
            channels: AudioChannels::Mono,
            sample_rate: 48_000,
            frame_duration_ms: 10,
            max_packet_bytes: 1000,
            max_decoded_samples: 480,
            jitter_target_ms: 20,
        },
        10,
        &mut bytes,
    )
    .unwrap();
    bytes
}

#[test]
fn original_input_connection_and_separate_microphone_consent_precede_native_setup() {
    runtime().block_on(async {
        let mut denied = Fixture::new().await;
        assert_eq!(
            denied.host(false, absent_launch()).unwrap_err().reason(),
            Error::Permission
        );
        denied.link.h.tick(&denied.cx, || true).unwrap();
        assert!(!denied.link.h.is_closed());
        assert!(!denied.input.control().is_stopped());
        let mut observer = Fixture::new().await;
        observer.link.selection.role = Role::Observe;
        assert_eq!(
            observer.host(true, absent_launch()).unwrap_err().reason(),
            Error::NotSelected
        );
        observer.link.h.tick(&observer.cx, || true).unwrap();
        assert!(!observer.link.h.is_closed());
        let mut original = Fixture::new().await;
        let foreign = Fixture::new().await;
        let host = Host::new(
            &mut original.link.h,
            original.lane.take().unwrap(),
            &foreign.input,
            &original.link.selection,
            true,
            absent_launch(),
        );
        let (reason, lane) = host.unwrap_err().into_parts();
        assert_eq!(
            reason,
            Error::Input(crate::input_quic::Error::WrongConnection)
        );
        original.link.h.tick(&original.cx, || true).unwrap();
        lane.check(&original.link.h).unwrap();
        let mut lane = lane;
        lane.retire(&mut original.link.h, &original.cx).unwrap();
        assert!(!foreign.link.h.is_closed());
        assert!(!foreign.input.control().is_stopped());
    });
}

#[test]
fn local_microphone_permission_refusal_retires_the_completed_pair_before_native_setup() {
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        let failure = Host::new_with_permission(
            &mut f.link.h,
            f.lane.take().unwrap(),
            &f.input,
            &f.link.selection,
            true,
            absent_launch(),
            Some(Arc::new(|| false)),
        )
        .unwrap_err();
        let (reason, lane) = failure.into_parts();
        assert_eq!(reason, Error::Permission);
        assert_eq!(lane.is_retired(&f.link.h), Ok(true));
        f.link.h.tick(&f.cx, || true).unwrap();
        assert!(!f.link.h.is_closed());
        assert!(!f.input.control().is_stopped());
        assert!(f.input_pair.0.completed_on(&f.link.h).is_ok());
    });
}

#[test]
fn local_microphone_permission_loss_while_idle_preserves_the_original_input() {
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        let permitted = Arc::new(AtomicBool::new(true));
        let check = permitted.clone();
        let mut host = Host::new_with_permission(
            &mut f.link.h,
            f.lane.take().unwrap(),
            &f.input,
            &f.link.selection,
            true,
            absent_launch(),
            Some(Arc::new(move || check.load(Ordering::Acquire))),
        )
        .unwrap();
        assert_eq!(host.state(), State::Idle);
        permitted.store(false, Ordering::Release);
        // Deliberately bypass Host::service: the retained original-connection
        // predicate must observe the local surface's loss on ordinary I/O.
        f.link.h.tick(&f.cx, || true).unwrap();
        assert!(host.control().is_stopped());
        assert_eq!(host.maintain(&mut f.link.h).unwrap(), State::Retired);
        assert_eq!(host.reason(), Some(Error::Permission));
        assert_eq!(host.statistics(), Statistics::default());
        assert!(host.retirement().is_none(), "no native source was started");
        assert!(!f.link.h.is_closed());
        assert!(!f.input.control().is_stopped());
        assert!(f.input_pair.0.completed_on(&f.link.h).is_ok());
        // A later healthy observation cannot revive this original attachment.
        permitted.store(true, Ordering::Release);
        assert_eq!(host.maintain(&mut f.link.h).unwrap(), State::Retired);
        assert!(host.control().is_stopped());
        f.link.h.tick(&f.cx, || true).unwrap();
    });
}

#[test]
fn local_microphone_permission_denial_never_waits_for_native_ownership() {
    let permitted = Arc::new(AtomicBool::new(true));
    let check = permitted.clone();
    let shared = Arc::new(Shared {
        permission: Some(Arc::new(move || check.load(Ordering::Acquire))),
        ..Shared::default()
    });
    assert_eq!(shared.check(), Ok(()));
    let native_owner = shared.native.lock().unwrap();
    permitted.store(false, Ordering::Release);
    let worker_shared = shared.clone();
    let surface_stop = Control(shared.clone()).downgrade();
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let permission = worker_shared.check_permission();
        surface_stop.request_stop();
        send.send(permission).unwrap();
    });
    let result = receive.recv_timeout(Duration::from_secs(1));
    // Always release and join, including a regression that waited on the
    // native owner. The test reports failure instead of stranding its thread.
    drop(native_owner);
    worker.join().unwrap();
    assert_eq!(result.unwrap(), Err(Error::Permission));
    permitted.store(true, Ordering::Release);
    assert_eq!(shared.check(), Err(Error::Closed));
}

#[test]
fn a_foreign_constructor_returns_the_original_live_proof_without_mutating_either_connection() {
    runtime().block_on(async {
        let mut original = Fixture::new().await;
        let mut foreign = Fixture::new().await;
        let original_before = original.link.h.usage();
        let foreign_before = foreign.link.h.usage();
        let failure = Host::new(
            &mut foreign.link.h,
            original.lane.take().unwrap(),
            &original.input,
            &original.link.selection,
            true,
            absent_launch(),
        )
        .unwrap_err();
        let (reason, lane) = failure.into_parts();
        assert_eq!(reason, Error::Transport(quic::Error::WrongRoute));
        original.link.h.tick(&original.cx, || true).unwrap();
        foreign.link.h.tick(&foreign.cx, || true).unwrap();
        assert_eq!(original.link.h.usage(), original_before);
        assert_eq!(foreign.link.h.usage(), foreign_before);
        lane.check(&original.link.h).unwrap();
        let mut host = Host::new(
            &mut original.link.h,
            lane,
            &original.input,
            &original.link.selection,
            true,
            absent_launch(),
        )
        .unwrap();
        host.retire(&mut original.link.h).unwrap();
        assert!(!original.input.control().is_stopped());
        assert!(!foreign.input.control().is_stopped());
    });
}

#[test]
fn a_panicking_connection_authorizer_fences_the_exact_host_owner() {
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        let mut host = f.host(true, absent_launch()).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            host.service(&mut f.link.h, || {
                panic!("test-only connection authorization panic")
            })
        }));
        assert!(result.is_err());
        assert_eq!(host.state(), State::Retired);
        assert!(host.control().is_stopped());
        assert!(f.link.h.is_closed());
    });
}

#[test]
fn semantically_invalid_authenticated_configuration_retires_only_audio_up() {
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        let mut host = f.host(true, absent_launch()).unwrap();
        // Its record framing, binding and stream role are valid; semantic
        // direction is independently refused without a connection handler error.
        f.link
            .c
            .send(
                &f.cx,
                Route::Stream(f.peer_control),
                &configuration(7, AudioDirection::Downlink),
                clock(&f.cx) + 1_000_000,
                || true,
            )
            .unwrap();
        let until = clock(&f.cx) + 1_000_000;
        while host.state() != State::Retired {
            assert!(clock(&f.cx) < until);
            f.drive(&host).await;
            host.service(&mut f.link.h, || true).unwrap();
        }
        assert_eq!(host.reason(), Some(Error::Protocol));
        assert!(
            host.retirement().is_none(),
            "no native process was ever started"
        );
        assert!(!f.link.h.is_closed());
        assert!(!f.input.control().is_stopped());
        assert!(f.input_pair.0.completed_on(&f.link.h).is_ok());
    });
}

#[test]
fn real_native_spawn_failure_never_acknowledges_and_preserves_original_input() {
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        let mut host = f.host(true, absent_launch()).unwrap();
        f.send(&configuration(7, AudioDirection::Uplink));
        let until = clock(&f.cx) + 1_000_000;
        while host.state() != State::Retired {
            assert!(clock(&f.cx) < until);
            f.drive(&host).await;
            host.service(&mut f.link.h, || true).unwrap();
        }
        assert_eq!(host.reason(), Some(Error::Native(native::Error::Spawn)));
        assert!(host.epoch.inflight.is_none());
        assert!(host.snapshot().is_none());
        let retirement = host
            .retirement()
            .expect("actual failed supervisor retained");
        while !retirement.is_complete() {
            assert!(
                clock(&f.cx) < until,
                "actual spawn failure must finish cleanup"
            );
            asupersync::time::sleep(f.cx.now(), Duration::from_millis(1)).await;
        }
        assert!(!f.link.h.is_closed());
        assert!(!f.input.control().is_stopped());
    });
}

#[test]
fn original_controller_revocation_and_foreign_retirement_cannot_cross_connections() {
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        let mut host = f.host(true, absent_launch()).unwrap();
        let mut foreign = Fixture::new().await;
        let before = foreign.link.h.usage();
        assert_eq!(
            host.retire(&mut foreign.link.h),
            Err(Error::WrongConnection)
        );
        assert_eq!(foreign.link.h.usage(), before);
        assert!(!host.control().is_stopped());
        f.input.control().stop(StopReason::LocalRevoke);
        assert!(
            host.permits_io(),
            "no ACK retained: preserve unrelated native I/O"
        );
        assert!(host.control().is_stopped());
        assert_eq!(host.maintain(&mut f.link.h).unwrap(), State::Retired);
        assert!(matches!(host.reason(), Some(Error::Authority(_))));
        assert!(!f.link.h.is_closed());
        assert!(!foreign.input.control().is_stopped());
    });
}

#[path = "tests/native.rs"]
pub(crate) mod device;

#[test]
fn unrelated_transport_dispatch_cannot_bypass_the_original_microphone_controller() {
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        let mut host = f.host(true, absent_launch()).unwrap();
        assert!(!host.control().is_stopped());
        f.input.control().stop(StopReason::LocalRevoke);
        // This helper deliberately omits Host::permits_io and Host::service.
        // The transport-retained gate must fence microphone output itself.
        f.link.h.tick(&f.cx, || true).unwrap();
        assert!(host.control().is_stopped());
        assert!(
            !f.link.h.is_closed(),
            "no retained ACK requires parent closure"
        );
        assert_eq!(host.maintain(&mut f.link.h).unwrap(), State::Retired);
        f.link.h.tick(&f.cx, || true).unwrap();
    });
}
