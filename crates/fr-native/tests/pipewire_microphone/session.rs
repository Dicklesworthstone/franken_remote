//! Real original-input ownership composed with the qualified endpoint/decoder.
//! Authority creation and the 500-Hz signal are fixtures. The input monitor,
//! independent recorder, virtual source and confined codec process are genuine.
use super::{OwnedChild, Server, State};
use fr_core::{
    audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig},
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
use fr_media::audio::{AudioAccessUnit, AudioEncoder, AudioPcmFrame};
use fr_native::{
    opus::{Encoder, process::Retirement},
    pipewire_mic::{
        session::{Authority, Channel, Error, Receiver},
        uplink::{ReceiveResult, RenderResult},
    },
};
use fr_wire::{
    attachment,
    audio::{self, AudioConfiguration, AudioPacket},
    negotiation::{Capability, ControlBinding, Role, Selection},
};
use std::{
    collections::VecDeque,
    fs,
    path::Path,
    time::{Duration, Instant},
};

const BINDING: u32 = 73;
const LEASE: u128 = 2;

fn host(origin: Instant) -> HostInstant {
    HostInstant::from_micros(1_000_000 + u64::try_from(origin.elapsed().as_micros()).unwrap())
}
fn input(now: HostInstant) -> InputSession {
    let credentials = InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(LEASE),
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
    // This fixture establishes a live controller; it does not claim a display
    // handshake, real approval UI or a public AudioUp command exists.
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
fn config() -> AudioStreamConfig {
    AudioStreamConfig::new(
        AudioDirection::Uplink,
        AudioGeneration::from_raw(7),
        AudioChannels::Mono,
        10,
        20,
    )
    .unwrap()
}
fn offer() -> AudioConfiguration {
    let config = config();
    AudioConfiguration {
        direction: config.direction(),
        generation: config.generation(),
        channels: config.channels(),
        sample_rate: config.sample_rate(),
        frame_duration_ms: 10,
        max_packet_bytes: 1000,
        max_decoded_samples: 480,
        jitter_target_ms: 20,
    }
}
fn configuration(offer: AudioConfiguration) -> Vec<u8> {
    let mut bytes = vec![0; audio::AUDIO_CONFIGURATION_RECORD_BYTES];
    let count = audio::encode_configuration(&offer, BINDING, &mut bytes).unwrap();
    bytes.truncate(count);
    bytes
}
fn authority(input: &InputSession, origin: Instant) -> Authority {
    Authority::from_input(input, channel(), &selection(), true, || host(origin)).unwrap()
}
fn receiver(input: &InputSession, origin: Instant, server: &Server) -> Receiver {
    let mut receiver = Receiver::new(
        authority(input, origin),
        &configuration(offer()),
        &server.socket,
        Path::new(env!("CARGO_BIN_EXE_fr-opus-worker")),
    )
    .unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while !receiver.poll_ready().unwrap() {
        assert!(Instant::now() < until, "original native setup deadline");
        std::thread::sleep(Duration::from_millis(1));
    }
    receiver
        .acknowledge(|bytes| {
            let reply = audio::decode_configured(bytes, BINDING).unwrap();
            assert!(reply.accepted);
            assert_eq!(reply.direction, AudioDirection::Uplink);
            assert_eq!(reply.generation, config().generation());
            Ok(())
        })
        .unwrap();
    receiver
}
fn renew(input: &mut InputSession, origin: Instant, nonce: &mut u128) {
    input
        .issue_observation_challenge(*nonce, host(origin))
        .unwrap();
    input.renew_observation(*nonce, host(origin)).unwrap();
    *nonce += 1;
    input.issue_control_challenge(*nonce, host(origin)).unwrap();
    input.renew_control(*nonce, host(origin)).unwrap();
    *nonce += 1;
}

#[test]
fn selection_consent_original_owner_and_packet_bound_precede_native_setup() {
    let origin = Instant::now();
    let input = input(host(origin));
    assert_eq!(
        Authority::from_input(&input, channel(), &selection(), false, || panic!(
            "denial cannot sample the clock"
        ))
        .unwrap_err(),
        Error::Permission
    );
    let mut observed = selection();
    observed.role = Role::Observe;
    assert_eq!(
        Authority::from_input(&input, channel(), &observed, true, || panic!(
            "an observer cannot receive a microphone"
        ))
        .unwrap_err(),
        Error::NotSelected
    );
    let mut foreign = channel();
    foreign.parent.remote_session = RemoteSessionId::from_raw(5);
    assert_eq!(
        Authority::from_input(&input, foreign, &selection(), true, || panic!(
            "foreign route cannot sample the clock"
        ))
        .unwrap_err(),
        Error::Binding
    );
    let mut too_large = offer();
    too_large.max_packet_bytes += 1;
    assert_eq!(
        Receiver::new(
            authority(&input, origin),
            &configuration(too_large),
            Path::new("/no-native-server"),
            Path::new("/no-codec-child")
        )
        .unwrap_err(),
        Error::Configuration
    );
    assert!(
        input.monitor().deadline(host(origin)).is_ok(),
        "microphone refusal cannot revoke input"
    );
}

#[test]
fn dropped_controller_or_microphone_authority_cannot_be_replaced_by_equal_ids() {
    let origin = Instant::now();
    let original = input(host(origin));
    let owner = authority(&original, origin);
    let stopped = owner.control();
    drop(original);
    let replacement = input(host(origin));
    let current = authority(&replacement, origin);
    assert!(!stopped.permitted());
    assert!(current.control().permitted());
    stopped.stop();
    assert!(current.control().permitted());
    let denied = current.control();
    drop(current);
    assert!(!denied.permitted());
    assert!(replacement.monitor().deadline(host(origin)).is_ok());
    drop(owner);
}

#[test]
fn native_setup_delay_cannot_restart_the_original_host_clock() {
    let input = input(HostInstant::ORIGIN);
    // The grant has only 20ms left on its original clock. Delaying native setup
    // cannot convert that remaining interval into a fresh provisional lease.
    let owner = Authority::from_input(&input, channel(), &selection(), true, || {
        HostInstant::from_micros(2_980_000)
    })
    .unwrap();
    let control = owner.control();
    assert!(control.permitted());
    std::thread::sleep(Duration::from_millis(25));
    assert!(matches!(
        Receiver::new(
            owner,
            &configuration(offer()),
            Path::new("/no-native-server"),
            Path::new("/no-codec-child")
        ),
        Err(Error::Authority(_))
    ));
    assert!(!control.permitted());
    assert!(input.monitor().is_revoked());
}

#[allow(clippy::cast_possible_truncation)]
fn packets() -> Vec<AudioAccessUnit> {
    let config = config();
    let mut encoder = Encoder::new();
    encoder.configure(config).unwrap();
    (0..381_u16)
        .map(|sequence| {
            let values: Vec<i16> = (0..480_u16)
                .map(|index| {
                    if sequence < 300 || sequence == 380 {
                        (9000.0 * (2.0 * std::f64::consts::PI * f64::from(index % 96) / 96.0).sin())
                            .round() as i16
                    } else {
                        0
                    }
                })
                .collect();
            let pcm = AudioPcmFrame::from_interleaved(
                config.generation(),
                config.channels(),
                1_000_000 + u64::from(sequence) * 480,
                &values,
            )
            .unwrap();
            encoder.submit_pcm(&pcm).unwrap();
            encoder.poll_packet().unwrap().unwrap()
        })
        .collect()
}
fn record(packet: &AudioAccessUnit) -> Vec<u8> {
    let mut bytes = vec![0; audio::AUDIO_PACKET_OVERHEAD + packet.payload().len()];
    let count = audio::encode_packet(
        &AudioPacket {
            direction: packet.direction(),
            generation: packet.generation(),
            sequence: packet.sequence(),
            timestamp_samples: packet.timestamp_samples(),
            duration_samples: packet.duration_samples(),
            payload: packet.payload(),
        },
        BINDING,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(count);
    bytes
}
fn reaped(retirement: &Retirement) {
    let until = Instant::now() + Duration::from_secs(3);
    while !retirement.is_complete() {
        assert!(
            Instant::now() < until,
            "original codec must actually be reaped"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(retirement.process_id(), None);
}
fn finish(server: &Server, capture: &mut OwnedChild) {
    fs::write(server.directory.join("finished"), []).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = capture.0.try_wait().unwrap() {
            assert!(
                status.success(),
                "see retained capture.stderr in {}",
                server.directory.display()
            );
            break;
        }
        assert!(
            Instant::now() < until,
            "independent recorder completion deadline"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let evidence = fs::read_to_string(server.directory.join("capture.stdout")).unwrap();
    assert!(evidence.contains("independent_capture_passed"));
    println!(
        "{}\nartifacts: {}",
        evidence.trim(),
        server.directory.display()
    );
}

#[derive(Clone, Copy, Debug)]
enum Fence {
    RevokeInput,
    DropInput,
    MicrophoneOnly,
}

/// Bounded timing evidence for a native deadline refusal. This never changes
/// a production deadline or tolerates a failed native turn.
#[derive(Debug)]
struct Cadence {
    origin: Instant,
    end: Instant,
    worst_pause: Duration,
    worst_turn: Duration,
    recent: VecDeque<(&'static str, u128, u128, u128)>,
}
impl Cadence {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            origin: now,
            end: now,
            worst_pause: Duration::ZERO,
            worst_turn: Duration::ZERO,
            recent: VecDeque::with_capacity(16),
        }
    }
    fn turn<T>(&mut self, stage: &'static str, action: impl FnOnce() -> T) -> T {
        let begin = Instant::now();
        let pause = begin.duration_since(self.end);
        let result = action();
        self.end = Instant::now();
        let duration = self.end.duration_since(begin);
        self.worst_pause = self.worst_pause.max(pause);
        self.worst_turn = self.worst_turn.max(duration);
        if self.recent.len() == 16 {
            self.recent.pop_front();
        }
        self.recent.push_back((
            stage,
            begin.duration_since(self.origin).as_micros(),
            duration.as_micros(),
            pause.as_micros(),
        ));
        result
    }
}

#[test]
#[ignore = "private PipeWire recorder + confined Opus worker; explicit native CI"]
fn real_controller_revoke_silences_queued_audio() {
    qualify(Fence::RevokeInput);
}

#[test]
#[ignore = "private PipeWire recorder + confined Opus worker; explicit native CI"]
fn real_controller_drop_is_terminal_for_an_equal_id_replacement() {
    qualify(Fence::DropInput);
}

#[test]
#[ignore = "private PipeWire recorder + confined Opus worker; explicit native CI"]
fn real_microphone_stop_silences_audio_without_revoking_input() {
    qualify(Fence::MicrophoneOnly);
}

fn qualify(fence: Fence) {
    let server = Server::start();
    let origin = Instant::now();
    let mut original = Some(input(host(origin)));
    let input_monitor = original.as_ref().unwrap().monitor();
    let mut receiver = receiver(original.as_ref().unwrap(), origin, &server);
    let control = receiver.control();
    let retirement = receiver.decoder_retirement();
    let packets = packets();
    assert_eq!(
        receiver.receive_record_at(
            &record(&packets[0]),
            Instant::now()
                .checked_sub(Duration::from_millis(101))
                .unwrap()
        ),
        Ok(ReceiveResult::Ignored)
    );
    assert_eq!(
        receiver.queued_packets(),
        0,
        "an old inbox receipt cannot acquire a new lifetime"
    );
    let mut capture = link(&server, &mut receiver);
    let mut nonce = play(
        &mut receiver,
        original.as_mut().unwrap(),
        origin,
        &packets,
        fence,
    );
    match fence {
        Fence::RevokeInput => original.as_mut().unwrap().revoke(),
        Fence::DropInput => drop(original.take()),
        Fence::MicrophoneOnly => control.stop(),
    }
    assert!(!control.permitted());
    assert!(receiver.service().is_err());
    assert_eq!(receiver.state(), State::Closed);
    assert_eq!(receiver.queued_packets(), 0);
    reaped(&retirement);
    if matches!(fence, Fence::MicrophoneOnly) {
        assert!(input_monitor.deadline(host(origin)).is_ok());
        renew(original.as_mut().unwrap(), origin, &mut nonce);
        assert!(
            !input_monitor.is_revoked(),
            "mic-only stop cannot revoke original input"
        );
        assert!(
            control.is_stopped(),
            "renewing input cannot renew microphone consent"
        );
    } else {
        assert!(input_monitor.is_revoked());
    }
    if matches!(fence, Fence::DropInput) {
        let replacement = input(host(origin));
        let mut live = Receiver::new(
            authority(&replacement, origin),
            &configuration(offer()),
            &server.socket,
            Path::new(env!("CARGO_BIN_EXE_fr-opus-worker")),
        )
        .unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        while !live.poll_ready().unwrap() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        control.stop();
        assert!(live.control().permitted());
        assert_eq!(live.state(), State::Ready);
        assert!(
            live.service().is_ok(),
            "old controls cannot stop the new native owner"
        );
        let next_retirement = live.decoder_retirement();
        live.stop();
        reaped(&next_retirement);
        assert!(replacement.monitor().deadline(host(origin)).is_ok());
    }
    finish(&server, &mut capture);
}

fn link(server: &Server, receiver: &mut Receiver) -> OwnedChild {
    let mut capture = server.recorder_mode(receiver.statistics().node_id, "opus");
    let until = Instant::now() + Duration::from_secs(2);
    while !server.directory.join("linked").exists() {
        receiver.service().unwrap();
        assert!(
            capture.0.try_wait().unwrap().is_none(),
            "see capture.stderr"
        );
        assert!(Instant::now() < until, "private consumer setup deadline");
        std::thread::sleep(Duration::from_millis(1));
    }
    let demand_start = Instant::now();
    while demand_start.elapsed() < Duration::from_millis(50) {
        receiver.service().unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
    capture
}

fn play(
    receiver: &mut Receiver,
    input: &mut InputSession,
    origin: Instant,
    packets: &[AudioAccessUnit],
    fence: Fence,
) -> u128 {
    let started = Instant::now();
    let mut sent = 0;
    let mut submitted = 0;
    let mut nonce = 10;
    let mut renew_at = started;
    let mut cadence = Cadence::new();
    while sent < packets.len() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "fixture delivery deadline"
        );
        let now = Instant::now();
        if now >= renew_at {
            cadence.turn("renew", || renew(input, origin, &mut nonce));
            renew_at = now + Duration::from_millis(500);
        }
        if started.elapsed() >= Duration::from_millis(u64::try_from(sent).unwrap() * 10) {
            assert_eq!(
                cadence.turn("receive", || receiver
                    .receive_record_at(&record(&packets[sent]), now)),
                Ok(ReceiveResult::Queued),
                "{fence:?} packet={sent} native={:?} timing={cadence:?}",
                receiver.statistics()
            );
            sent += 1;
            if sent == packets.len() {
                // Fence the final genuine tone at its original packet cadence,
                // before servicing it. Waiting past its intended source time
                // would correctly classify it as already concealed/obsolete.
                break;
            }
        }
        if matches!(
            cadence
                .turn("service", || receiver.service())
                .unwrap_or_else(|error| panic!(
                    "{fence:?} sent={sent} elapsed={:?} native={:?} timing={cadence:?}: {error:?}",
                    started.elapsed(),
                    receiver.statistics()
                )),
            RenderResult::Submitted { .. }
        ) {
            submitted += 1;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(sent, packets.len());
    assert!(submitted > 100);
    assert!(receiver.statistics().submitted_frames > 48_000);
    println!(
        "{fence:?} native worker worst pause={:?} worst turn={:?}",
        cadence.worst_pause, cadence.worst_turn
    );
    assert!(
        receiver.queued_packets() > 0,
        "fresh audible work must remain queued at the fence"
    );
    nonce
}
