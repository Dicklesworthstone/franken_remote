#![cfg(target_os = "linux")]
//! The exact pure parent implementation with real installed device+codec images.
//! Controller establishment is a policy fixture. The private `PipeWire` daemon,
//! source identity, child freezes, kernel exits and independent graph queries
//! are real. These tests make no claim that an application recorded audio.
#[path = "../src/microphone_process.rs"]
pub mod microphone_process;

use fr_core::{
    audio::{AudioChannels, AudioDirection, AudioGeneration},
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, InputLeaseId,
        InputTicketId, OsSessionId, RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{DesktopPoint, InputBounds, InputCredentials, InputView},
    input_submission::{Capabilities, Capability as InputCapability, InputSession},
    limits::ProtocolLimits,
    time::HostInstant,
};
use fr_media::audio::authority::HostController;
use fr_wire::{
    attachment,
    audio::{self, AudioConfiguration, AudioPacket},
    negotiation::{Capability, ControlBinding, Role, Selection},
};
use microphone_process::{
    Admission, Channel, ChildExit, Error, Launch, Process, Retirement, State,
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const BINDING: u32 = 73;
fn host(origin: Instant) -> HostInstant {
    HostInstant::from_micros(1_000_000 + u64::try_from(origin.elapsed().as_micros()).unwrap())
}
fn input(now: HostInstant) -> InputSession {
    let credentials = InputCredentials {
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
    let mut policy = SessionAuthority::new(credentials.session, AuthorityPolicy::plan_defaults());
    policy.mark_capabilities_checked().unwrap();
    policy.authorize_observation(now).unwrap();
    policy.mark_view_ready(now).unwrap();
    policy.grant_lease(credentials.lease, now).unwrap();
    policy
        .issue_input_ticket(credentials.lease, credentials.ticket, now)
        .unwrap();
    InputSession::new(
        policy,
        credentials,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default().with(InputCapability::Keys),
        now,
    )
    .unwrap()
}
fn selection() -> Selection {
    let mut capabilities: Vec<_> = [
        (fr_wire::audio_up::CAPABILITY, fr_wire::audio_up::VERSION),
        (attachment::CAPABILITY, attachment::VERSION),
        (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
    ]
    .into_iter()
    .map(|(name, version)| Capability {
        name: name.into(),
        version,
        required: false,
    })
    .collect();
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    Selection {
        version: 0,
        profile: 1,
        profile_version: 0,
        role: Role::RequestControl,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities,
        picture: None,
    }
}
fn channel() -> Channel {
    Channel {
        parent: ControlBinding {
            id: 9,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(1),
            remote_session: RemoteSessionId::from_raw(1),
        },
        id: BINDING,
        packet_maximum: audio::AUDIO_PACKET_OVERHEAD + 1000,
    }
}
fn configuration() -> Vec<u8> {
    let offer = AudioConfiguration {
        direction: AudioDirection::Uplink,
        generation: AudioGeneration::from_raw(7),
        channels: AudioChannels::Mono,
        sample_rate: 48_000,
        frame_duration_ms: 10,
        max_packet_bytes: 1000,
        max_decoded_samples: 480,
        jitter_target_ms: 20,
    };
    let mut bytes = vec![0; audio::AUDIO_CONFIGURATION_RECORD_BYTES];
    audio::encode_configuration(&offer, BINDING, &mut bytes).unwrap();
    bytes
}
fn packet() -> Vec<u8> {
    // Valid opaque packet record for admission/no-consumer tests; this fixture
    // does not assert decoding or audible output from these three payload bytes.
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
        BINDING,
        &mut bytes,
    )
    .unwrap();
    bytes
}
fn expires(now: HostInstant) -> HostInstant {
    HostInstant::from_micros(now.as_micros() + 100_000)
}
fn absent_launch() -> Launch {
    Launch::new(
        Path::new("/no-native-device"),
        Path::new("/no-native-codec"),
        Path::new("/no-native-server/pipewire-0"),
    )
    .unwrap()
}
#[test]
fn independent_consent_exact_scope_and_native_profile_precede_every_child() {
    let origin = Instant::now();
    let owner = input(host(origin));
    let denied = Process::start(
        HostController::from_input(&owner),
        channel(),
        &selection(),
        false,
        &configuration(),
        absent_launch(),
        || panic!("denied microphone must not sample authority clock"),
    );
    assert_eq!(denied.unwrap_err(), Error::Permission);
    let mut observed = selection();
    observed.role = Role::Observe;
    assert_eq!(
        Process::start(
            HostController::from_input(&owner),
            channel(),
            &observed,
            true,
            &configuration(),
            absent_launch(),
            || panic!("observer cannot start a microphone")
        )
        .unwrap_err(),
        Error::NotSelected
    );
    let mut foreign = channel();
    foreign.parent.remote_session = RemoteSessionId::from_raw(5);
    assert_eq!(
        Process::start(
            HostController::from_input(&owner),
            foreign,
            &selection(),
            true,
            &configuration(),
            absent_launch(),
            || panic!("foreign route cannot start a microphone")
        )
        .unwrap_err(),
        Error::Binding
    );
    let mut too_small = channel();
    too_small.packet_maximum -= 1;
    assert_eq!(
        Process::start(
            HostController::from_input(&owner),
            too_small,
            &selection(),
            true,
            &configuration(),
            absent_launch(),
            || panic!("bad packet ceiling precedes native work")
        )
        .unwrap_err(),
        Error::Protocol
    );
    let proof = HostController::from_input(&owner);
    drop(owner);
    assert!(matches!(
        Process::start(
            proof,
            channel(),
            &selection(),
            true,
            &configuration(),
            absent_launch(),
            || host(origin)
        ),
        Err(Error::Authority(_))
    ));
}

static NEXT: AtomicU64 = AtomicU64::new(0);
// Keep real native qualification within the global bounded owner admission
// limit even with parallel cargo test scheduling. Every passing case observes
// both original OS waits before releasing this test-only resource guard.
static NATIVE_CASE: Mutex<()> = Mutex::new(());
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Server {
    directory: PathBuf,
    socket: PathBuf,
    _child: OwnedChild,
}
impl Server {
    fn start() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "fr-host-mic-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let mut child = OwnedChild(Command::new("pipewire").env("XDG_RUNTIME_DIR", &directory).env("PIPEWIRE_RUNTIME_DIR", &directory)
            .env_remove("PIPEWIRE_REMOTE").env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(Stdio::null()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn()
            .expect("actual supervision qualification requires installed PipeWire; no skip fallback"));
        let socket = directory.join("pipewire-0");
        let until = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(child.0.try_wait().unwrap().is_none());
            assert!(Instant::now() < until, "real private daemon startup");
            std::thread::sleep(Duration::from_millis(2));
        }
        Self {
            directory,
            socket,
            _child: child,
        }
    }
    fn node_exists(&self, node: u32) -> bool {
        let output = Command::new("pw-dump")
            .env("XDG_RUNTIME_DIR", &self.directory)
            .env("PIPEWIRE_RUNTIME_DIR", &self.directory)
            .env("PIPEWIRE_REMOTE", "pipewire-0")
            .stderr(Stdio::inherit())
            .output()
            .expect("independent registry query");
        assert!(
            output.status.success(),
            "pw-dump failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        rows.as_array().unwrap().iter().any(|row| {
            row["id"].as_u64() == Some(u64::from(node)) && row["type"] == "PipeWire:Interface:Node"
        })
    }
    fn removed(&self, node: u32) {
        let until = Instant::now() + Duration::from_secs(2);
        while self.node_exists(node) {
            assert!(
                Instant::now() < until,
                "exact original source survived actual device exit"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
fn device_image() -> PathBuf {
    std::env::var_os("FR_TEST_MICROPHONE_WORKER").map(PathBuf::from)
        .or_else(|| option_env!("CARGO_BIN_EXE_fr-microphone-worker").map(PathBuf::from))
        .expect("build fr-microphone-worker and set FR_TEST_MICROPHONE_WORKER to the installed absolute image")
}
fn codec_image() -> PathBuf {
    std::env::var_os("FR_TEST_OPUS_WORKER")
        .map(PathBuf::from)
        .or_else(|| option_env!("CARGO_BIN_EXE_fr-opus-worker").map(PathBuf::from))
        .expect("build fr-opus-worker and set FR_TEST_OPUS_WORKER to the installed absolute image")
}
fn start(owner: &InputSession, origin: Instant, server: &Server) -> Process {
    Process::start(
        HostController::from_input(owner),
        channel(),
        &selection(),
        true,
        &configuration(),
        Launch::new(&device_image(), &codec_image(), &server.socket).unwrap(),
        || host(origin),
    )
    .unwrap()
}
fn ready(process: &Process) -> u32 {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        match process.poll_ready() {
            Ok(Some(snapshot)) => return snapshot.node_id,
            Ok(None) => {
                assert!(Instant::now() < until, "original two-second setup");
            }
            Err(error) => panic!(
                "actual device/codec setup failed: {error:?}; {:?}",
                process.error()
            ),
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn commit(process: &mut Process) {
    let original = process.ready_acknowledgement().unwrap().unwrap();
    assert_eq!(process.ready_acknowledgement().unwrap(), Some(original));
    let mut callbacks = 0;
    process
        .acknowledge(|record| {
            callbacks += 1;
            assert_eq!(record, original);
            let ack = audio::decode_configured(record, BINDING).unwrap();
            assert!(ack.accepted);
            assert_eq!(ack.generation, AudioGeneration::from_raw(7));
            assert_eq!(ack.actual_channels, AudioChannels::Mono);
            assert_eq!(ack.actual_sample_rate, 48_000);
            Ok(())
        })
        .unwrap();
    assert_eq!(callbacks, 1);
    assert!(process.ready_acknowledgement().unwrap().is_none());
    let until = Instant::now() + Duration::from_millis(200);
    while process.state() != State::Running {
        assert_eq!(
            process.state(),
            State::Committing,
            "commit failed: {:?}",
            process.error()
        );
        assert!(Instant::now() < until, "matching private commit reply");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn reaped(retirement: &Retirement) {
    let until = Instant::now() + Duration::from_secs(2);
    // Only read actual receipts. Do not call Process::state/control policy:
    // idle revocation must be observed by the background supervisor itself.
    while !retirement.is_complete() {
        assert!(
            Instant::now() < until,
            "both original OS children must be reaped: {:?}",
            retirement.receipts()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let receipts = retirement.receipts();
    assert_eq!(receipts.device, ChildExit::Reaped);
    assert_eq!(receipts.codec, ChildExit::Reaped);
}
fn freeze(pid: u32) {
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap(),
        rustix::process::Signal::STOP,
    )
    .unwrap();
    let until = Instant::now() + Duration::from_millis(50);
    loop {
        let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        if status
            .lines()
            .any(|line| line.starts_with("State:") && line.contains('T'))
        {
            break;
        }
        assert!(Instant::now() < until, "kernel must confirm the freeze");
    }
}

#[test]
#[ignore = "requires real PipeWire plus installed fr-microphone-worker/fr-opus-worker; explicitly run in native CI"]
fn original_controller_drop_reaps_both_real_siblings_and_removes_exact_source_while_idle() {
    let _serial = NATIVE_CASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = Server::start();
    let origin = Instant::now();
    let owner = input(host(origin));
    let mut process = start(&owner, origin, &server);
    let node = ready(&process);
    assert!(server.node_exists(node));
    let now = host(origin);
    assert_eq!(
        process.receive_record_until(&packet(), now, expires(now)),
        Err(Error::NotReady)
    );
    commit(&mut process);
    let now = host(origin);
    assert_eq!(
        process
            .receive_record_until(&packet(), now, expires(now))
            .unwrap(),
        Admission::Queued
    );
    assert_eq!(
        process.receive_record_until(&packet(), now, expires(now)),
        Err(Error::Busy)
    );
    let until = Instant::now() + Duration::from_millis(100);
    loop {
        if let Some(outcome) = process.poll_packet().unwrap() {
            assert_eq!(
                outcome,
                microphone_process::PacketOutcome::Ignored,
                "no real consumer means no audio clock or packet custody"
            );
            break;
        }
        assert!(Instant::now() < until, "bounded original packet IPC reply");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(process.snapshot().unwrap().queued_packets, 0);
    let now = host(origin);
    let old = HostInstant::from_micros(now.as_micros() - 90_000);
    assert_eq!(
        process
            .receive_record_until(
                &packet(),
                old,
                HostInstant::from_micros(old.as_micros() + 20_000)
            )
            .unwrap(),
        Admission::Ignored
    );
    assert_eq!(process.state(), State::Running);
    let retirement = process.retirement();
    let original_cap = owner.monitor().deadline(host(origin)).unwrap();
    let revoked = Instant::now();
    drop(owner);
    reaped(&retirement);
    assert!(revoked.elapsed() < Duration::from_millis(500));
    assert!(
        host(origin) < original_cap,
        "revocation must retire before ordinary original expiry"
    );
    assert_eq!(
        process.error(),
        Some(Error::Authority(
            fr_core::input_submission::Refusal::Revoked
        ))
    );
    server.removed(node);
    assert!(process.control().is_stopped());
}

#[test]
#[ignore = "requires real PipeWire and both installed worker images; explicitly run in native CI"]
fn failed_original_route_admission_never_arms_output_and_reaps_both_children() {
    let _serial = NATIVE_CASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = Server::start();
    let origin = Instant::now();
    let owner = input(host(origin));
    let mut process = start(&owner, origin, &server);
    let node = ready(&process);
    assert!(server.node_exists(node));
    let retirement = process.retirement();
    let mut observed = Vec::new();
    let mut calls = 0;
    assert_eq!(
        process.acknowledge(|bytes| {
            calls += 1;
            observed.extend_from_slice(bytes);
            Err(())
        }),
        Err(Error::AcknowledgementUnknown)
    );
    assert_eq!(calls, 1);
    assert!(
        audio::decode_configured(&observed, BINDING)
            .unwrap()
            .accepted
    );
    reaped(&retirement);
    server.removed(node);
    assert!(
        owner.monitor().deadline(host(origin)).is_ok(),
        "microphone-only refusal must not revoke input"
    );
    assert_eq!(process.snapshot().unwrap().submitted_frames, 0);
}

#[test]
#[ignore = "requires real PipeWire, Linux process signals and both installed workers; explicitly run in native CI"]
fn frozen_device_is_bounded_by_parent_health_and_both_siblings_are_actually_reaped() {
    let _serial = NATIVE_CASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = Server::start();
    let origin = Instant::now();
    let owner = input(host(origin));
    let mut process = start(&owner, origin, &server);
    let node = ready(&process);
    assert!(server.node_exists(node));
    commit(&mut process);
    let retirement = process.retirement();
    let ids = retirement
        .child_ids()
        .expect("both original Child identities after readiness");
    let device = ids.device;
    let codec = ids.codec;
    assert_ne!(device, codec);
    freeze(device);
    let start = Instant::now();
    reaped(&retirement);
    assert!(start.elapsed() < Duration::from_millis(500));
    assert_eq!(
        process.error(),
        Some(Error::Timeout),
        "the frozen device must end at its original health-exchange deadline"
    );
    server.removed(node);
    assert!(!Path::new(&format!("/proc/{device}")).exists());
    assert!(!Path::new(&format!("/proc/{codec}")).exists());
    assert!(owner.monitor().deadline(host(origin)).is_ok());
}

#[test]
#[ignore = "requires real PipeWire, Linux process signals and both installed workers; explicitly run in native CI"]
fn frozen_codec_is_reaped_on_idle_authority_loss_and_old_fences_cannot_stop_replacement() {
    let _serial = NATIVE_CASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = Server::start();
    let origin = Instant::now();
    let owner = input(host(origin));
    let mut process = start(&owner, origin, &server);
    let node = ready(&process);
    assert!(server.node_exists(node));
    commit(&mut process);
    let old = process.control();
    let retirement = process.retirement();
    freeze(
        retirement
            .child_ids()
            .expect("actual original codec Child identity")
            .codec,
    );
    let original_cap = owner.monitor().deadline(host(origin)).unwrap();
    let revoked = Instant::now();
    drop(owner);
    reaped(&retirement);
    assert!(revoked.elapsed() < Duration::from_millis(500));
    assert!(
        host(origin) < original_cap,
        "revocation must retire before ordinary original expiry"
    );
    assert_eq!(
        process.error(),
        Some(Error::Authority(
            fr_core::input_submission::Refusal::Revoked
        ))
    );
    server.removed(node);
    let replacement = input(host(origin));
    let mut next = start(&replacement, origin, &server);
    let next_node = ready(&next);
    assert!(server.node_exists(next_node));
    commit(&mut next);
    old.stop();
    retirement.stop();
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(next.state(), State::Running);
    assert!(server.node_exists(next_node));
    assert!(replacement.monitor().deadline(host(origin)).is_ok());
    let next_retirement = next.retirement();
    next.stop();
    reaped(&next_retirement);
    server.removed(next_node);
}

#[test]
#[ignore = "requires real PipeWire and both installed worker images; explicitly run in native CI"]
fn malformed_owned_record_and_future_original_receipt_are_terminal() {
    let _serial = NATIVE_CASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = Server::start();
    let origin = Instant::now();
    let owner = input(host(origin));
    for future in [false, true] {
        let mut process = start(&owner, origin, &server);
        let node = ready(&process);
        assert!(server.node_exists(node));
        commit(&mut process);
        let retirement = process.retirement();
        let now = host(origin);
        let error = if future {
            let bad = HostInstant::from_micros(now.as_micros() + 1_000_000);
            process.receive_record_until(&packet(), bad, expires(bad))
        } else {
            process.receive_record_until(&[0; 12], now, expires(now))
        };
        assert_eq!(
            error,
            Err(if future {
                Error::Clock
            } else {
                Error::Protocol
            })
        );
        reaped(&retirement);
        server.removed(node);
        assert!(owner.monitor().deadline(host(origin)).is_ok());
    }
}
