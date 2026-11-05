//! These cases require the actual installed device/codec images and private
//! `PipeWire` daemon. They prove network/native lifecycle, not audible latency or
//! host-application recording. No consumer is attached to the real source.
use super::*;
use fr_wire::audio::{AudioPacket, AudioStop};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

static NEXT: AtomicU64 = AtomicU64::new(0);
static SERIAL: Mutex<()> = Mutex::new(());
pub(crate) struct Server {
    socket: PathBuf,
    child: Child,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Server {
    pub(crate) fn start() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "fr-host-mic-quic-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let child = Command::new("pipewire")
            .env("XDG_RUNTIME_DIR", &directory)
            .env("PIPEWIRE_RUNTIME_DIR", &directory)
            .env_remove("PIPEWIRE_REMOTE")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("real private PipeWire daemon required");
        let mut server = Self {
            socket: directory.join("pipewire-0"),
            child,
        };
        let until = Instant::now() + Duration::from_secs(5);
        while !server.socket.exists() {
            assert!(server.child.try_wait().unwrap().is_none());
            assert!(Instant::now() < until, "actual native daemon startup");
            std::thread::sleep(Duration::from_millis(1));
        }
        server
    }
    pub(crate) fn launch(&self) -> native::Launch {
        let device = std::env::var_os("FR_TEST_MICROPHONE_WORKER")
            .map(PathBuf::from)
            .expect("FR_TEST_MICROPHONE_WORKER must name the actual installed image");
        let codec = std::env::var_os("FR_TEST_OPUS_WORKER")
            .map(PathBuf::from)
            .expect("FR_TEST_OPUS_WORKER must name the actual installed image");
        native::Launch::new(&device, &codec, &self.socket).unwrap()
    }
}
fn stop(generation: u64) -> Vec<u8> {
    stop_with_reason(generation, AudioStopReason::UserMute)
}
fn stop_with_reason(generation: u64, reason: AudioStopReason) -> Vec<u8> {
    let mut bytes = vec![0; audio::AUDIO_STOP_RECORD_BYTES];
    audio::encode_stop(
        &AudioStop {
            direction: AudioDirection::Uplink,
            generation: AudioGeneration::from_raw(generation),
            reason,
        },
        10,
        &mut bytes,
    )
    .unwrap();
    bytes
}
async fn running(f: &mut Fixture, host: &mut Host, generation: u64) {
    let until = clock(&f.cx) + 1_000_000;
    let mut acknowledged = false;
    while host.state() != State::Running || !acknowledged {
        assert!(
            clock(&f.cx) < until,
            "real readiness/transport ACK/private Commit: {:?}",
            host.reason()
        );
        f.drive(host).await;
        host.service(&mut f.link.h, || true).unwrap();
        f.peer
            .dispatch(
                &mut f.link.c,
                &f.cx,
                || true,
                |_| true,
                |_, bytes, receipt| {
                    assert!(receipt.is_none());
                    if audio::record_kind(bytes) == Some(0x61) {
                        let ack = audio::decode_configured(bytes, 10).unwrap();
                        assert!(ack.accepted);
                        assert_eq!(ack.generation.as_raw(), generation);
                        acknowledged = true;
                    }
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
    }
}
async fn reaped(cx: &Cx, retirement: &native::Retirement) {
    let until = clock(cx) + 1_000_000;
    while !retirement.is_complete() {
        assert!(
            clock(cx) < until,
            "both actual original children must exit: {:?}",
            retirement.receipts()
        );
        asupersync::time::sleep(cx.now(), Duration::from_millis(1)).await;
    }
    let receipts = retirement.receipts();
    assert_eq!(receipts.device, native::ChildExit::Reaped);
    assert_eq!(receipts.codec, native::ChildExit::Reaped);
}

#[test]
#[ignore = "requires real PipeWire and installed device/Opus worker images; explicit native race qualification"]
fn consent_loss_between_receive_rounds_retires_the_real_ack_without_closing_input() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = Server::start();
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        let mut host = f.host(true, server.launch()).unwrap();
        f.send(&configuration(7, AudioDirection::Uplink));
        let until = clock(&f.cx) + 1_000_000;
        while host.epoch.process.is_none() {
            assert!(clock(&f.cx) < until);
            f.drive(&host).await;
            host.receive(&mut f.link.h, || true).unwrap();
        }
        while host
            .epoch
            .process
            .as_ref()
            .unwrap()
            .poll_ready()
            .unwrap()
            .is_none()
        {
            assert!(clock(&f.cx) < until);
            asupersync::time::sleep(f.cx.now(), Duration::from_millis(1)).await;
        }
        host.service(&mut f.link.h, || true).unwrap();
        assert_eq!(host.statistics().acknowledgements_admitted, 1);
        assert!(
            host.epoch.inflight.is_some(),
            "original positive ACK is still retained before I/O"
        );
        let retirement = host.retirement().unwrap();
        let control = host.control();
        let mut checks = 0;
        assert_eq!(
            host.service(&mut f.link.h, || {
                checks += 1;
                // First check is tick; second begins bounded receive. The next
                // receive round observes the actual local consent fence and must
                // retire only AudioUp before any native transport I/O can occur.
                if checks == 2 {
                    control.stop();
                }
                true
            })
            .unwrap(),
            State::Retired
        );
        assert!(checks >= 3);
        reaped(&f.cx, &retirement).await;
        assert!(!f.link.h.is_closed());
        assert!(!f.input.control().is_stopped());
        f.link.h.tick(&f.cx, || true).unwrap();
    });
}

#[test]
#[ignore = "requires real PipeWire and installed device/Opus worker images; run explicitly in native CI"]
fn real_ack_backpressure_does_not_commit_or_replace_the_original_native_reply() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = Server::start();
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        let mut host = f.host(true, server.launch()).unwrap();
        f.send(&configuration(7, AudioDirection::Uplink));
        let until = clock(&f.cx) + 1_000_000;
        while host.epoch.process.is_none() {
            assert!(clock(&f.cx) < until);
            f.drive(&host).await;
            // Run the production bounded receive stage without the following
            // ACK-send stage. This establishes a deterministic real Ready /
            // transport-backpressure boundary without replacing a worker or
            // depending on which OS thread happens to run after spawning it.
            host.receive(&mut f.link.h, || true).unwrap();
        }
        assert_eq!(host.statistics().acknowledgements_admitted, 0);
        // Leave the network coordinator idle while the actual two children
        // reach Ready. Native readiness alone must not arm the virtual source.
        while host
            .epoch
            .process
            .as_ref()
            .unwrap()
            .poll_ready()
            .unwrap()
            .is_none()
        {
            assert!(clock(&f.cx) < until);
            asupersync::time::sleep(f.cx.now(), Duration::from_millis(1)).await;
        }
        let original = host
            .epoch
            .process
            .as_ref()
            .unwrap()
            .ready_acknowledgement()
            .unwrap()
            .unwrap();
        let deadline = host.epoch.setup_until.unwrap();
        assert_eq!(
            host.lane
                .send_control_prepared(
                    &mut f.link.h,
                    &f.cx,
                    &stop(6),
                    clock(&f.cx) + 500_000,
                    || true,
                    || Ok::<_, ()>(())
                )
                .unwrap(),
            SendAdmission::Accepted
        );
        let usage = f.link.h.usage();
        for _ in 0..3 {
            assert_eq!(host.service(&mut f.link.h, || true).unwrap(), State::Ready);
            assert_eq!(host.epoch.setup_until, Some(deadline));
            assert_eq!(host.epoch.acknowledgement, Some(original));
            assert_eq!(host.statistics().acknowledgements_admitted, 0);
            assert_eq!(
                f.link.h.usage().retained_send_records,
                usage.retained_send_records
            );
        }
        running(&mut f, &mut host, 7).await;
        assert_eq!(host.statistics().acknowledgements_admitted, 1);
        assert!(
            host.epoch
                .process
                .as_ref()
                .unwrap()
                .ready_acknowledgement()
                .unwrap()
                .is_none()
        );
        let retirement = host.retirement().unwrap();
        host.retire(&mut f.link.h).unwrap();
        reaped(&f.cx, &retirement).await;
        assert!(!f.link.h.is_closed());
        assert!(!f.input.control().is_stopped());
    });
}

async fn original_receipt_expiry(f: &mut Fixture, host: &mut Host) {
    let mut bytes = vec![0; audio::AUDIO_PACKET_OVERHEAD + 3];
    audio::encode_packet(
        &AudioPacket {
            direction: AudioDirection::Uplink,
            generation: AudioGeneration::from_raw(7),
            sequence: 0,
            timestamp_samples: 0,
            duration_samples: 480,
            payload: &[0xf8, 0xff, 0xfe],
        },
        10,
        &mut bytes,
    )
    .unwrap();
    f.peer
        .send_packet_prepared(
            &mut f.link.c,
            &f.cx,
            &bytes,
            clock(&f.cx) + 100_000,
            || true,
            || Ok::<_, ()>(()),
        )
        .unwrap();
    // Pull the actual encrypted datagram into the transport's bounded slot
    // with readiness disabled, preserving the timer-driver receipt.
    let until = clock(&f.cx) + 100_000;
    let mut original_receipt = None;
    while original_receipt.is_none() {
        assert!(clock(&f.cx) < until, "actual datagram never arrived");
        f.drive(host).await;
        host.lane
            .dispatch(
                &mut f.link.h,
                &f.cx,
                || host.permits_io(),
                |_| true,
                |_, _, receipt| {
                    original_receipt = receipt;
                    Ok(Disposition::Blocked)
                },
            )
            .unwrap();
    }
    let original_receipt = original_receipt.unwrap();
    while clock(&f.cx) < original_receipt.expires_at_micros() {
        asupersync::time::sleep(f.cx.now(), Duration::from_millis(1)).await;
    }
    host.service(&mut f.link.h, || true).unwrap();
    assert_eq!(
        host.statistics().packets_queued,
        0,
        "service must not freshen old custody"
    );
    f.peer
        .send_packet_prepared(
            &mut f.link.c,
            &f.cx,
            &bytes,
            clock(&f.cx) + 100_000,
            || true,
            || Ok::<_, ()>(()),
        )
        .unwrap();
    let until = clock(&f.cx) + 200_000;
    while host.statistics().packets_ignored == 0 {
        assert!(
            clock(&f.cx) < until,
            "real process must return bounded no-consumer outcome"
        );
        f.drive(host).await;
        host.service(&mut f.link.h, || true).unwrap();
    }
    assert_eq!(host.statistics().packets_queued, 1);
    assert_eq!(
        host.statistics().packets_admitted,
        0,
        "no actual source consumer means no audio-clock admission"
    );
}

#[test]
#[ignore = "requires real PipeWire and both installed workers; explicit native lifecycle qualification"]
fn real_datagram_receipts_expire_and_explicit_retalk_waits_for_both_original_children() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = Server::start();
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        let mut host = f.host(true, server.launch()).unwrap();
        f.send(&configuration(7, AudioDirection::Uplink));
        running(&mut f, &mut host, 7).await;
        original_receipt_expiry(&mut f, &mut host).await;
        for (generation, reason) in [
            (7, AudioStopReason::UserMute),
            (8, AudioStopReason::DeviceChanged),
        ] {
            let old = host.retirement().unwrap();
            f.send(&stop_with_reason(generation, reason));
            let until = clock(&f.cx) + 500_000;
            while host.epoch.process.is_some() {
                assert!(clock(&f.cx) < until);
                f.drive(&host).await;
                host.service(&mut f.link.h, || true).unwrap();
                assert!(
                    !host.control().is_stopped(),
                    "{reason:?} must stop its epoch without revoking consent: {:?}",
                    host.reason()
                );
            }
            reaped(&f.cx, &old).await;
            assert_eq!(host.maintain(&mut f.link.h).unwrap(), State::Idle);
            assert_eq!(host.reason(), None);
            assert!(!f.link.h.is_closed());
            assert!(!f.input.control().is_stopped());
            while !f.peer.control_send_drained(&f.link.c).unwrap() {
                f.drive(&host).await;
            }
            f.send(&configuration(generation, AudioDirection::Uplink));
            let until = clock(&f.cx) + 100_000;
            while clock(&f.cx) < until {
                f.drive(&host).await;
                host.service(&mut f.link.h, || true).unwrap();
            }
            assert!(
                host.epoch.process.is_none(),
                "old generation cannot recreate an epoch"
            );
            assert_eq!(host.statistics().configurations_started, generation - 6);
            f.send(&configuration(generation + 1, AudioDirection::Uplink));
            running(&mut f, &mut host, generation + 1).await;
            assert_eq!(host.statistics().configurations_started, generation - 5);
            assert!(old.is_complete());
        }
        let next = host.retirement().unwrap();
        host.control().stop();
        reaped(&f.cx, &next).await;
        assert_eq!(host.maintain(&mut f.link.h).unwrap(), State::Retired);
        assert!(!f.link.h.is_closed());
        assert!(!f.input.control().is_stopped());
    });
}

#[test]
#[ignore = "requires real PipeWire and installed device/Opus worker images"]
fn unrelated_transport_tick_retires_the_real_ack_before_further_native_io() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = Server::start();
    runtime().block_on(async {
        let mut f = Fixture::new().await;
        let mut host = f.host(true, server.launch()).unwrap();
        f.send(&configuration(7, AudioDirection::Uplink));
        let until = clock(&f.cx) + 1_000_000;
        while host.statistics().acknowledgements_admitted == 0 {
            assert!(clock(&f.cx) < until, "actual native ACK must be admitted");
            f.drive(&host).await;
            host.service(&mut f.link.h, || true).unwrap();
        }
        assert!(host.epoch.inflight.is_some());
        let retirement = host.retirement().unwrap();
        host.control().stop();
        // No Host callback services this operation. The immutable original
        // guard must retire its queued ACK before this unrelated helper returns.
        f.link.h.tick(&f.cx, || true).unwrap();
        assert!(host.lane.is_retired(&f.link.h).unwrap());
        assert!(!f.link.h.is_closed());
        assert!(!f.input.control().is_stopped());
        assert_eq!(host.maintain(&mut f.link.h).unwrap(), State::Retired);
        reaped(&f.cx, &retirement).await;
    });
}
