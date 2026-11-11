//! Real control-grant and audio codecs with deterministic explicit clocks.
//! Payload bytes are test input; these tests do not claim native Opus capture.
use super::*;
use crate::{
    control_grant::RequestControl,
    input::{Policy, PresentedObservation, StopReason},
};
use fr_core::{
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, InputTicketId,
        OsSessionId, RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{DesktopPoint, InputBounds, InputView},
    input_submission::{Capabilities, Capability},
    limits::ProtocolLimits,
};
use fr_media::freshness::{ClockCorrelation, ClockPolicy, ClockSample};
use fr_wire::{
    control::{self, Granted, Request, Target},
    input::{InputDelivery, InputDirection},
    negotiation::{Capability as SelectedCapability, Role},
};
const H: u64 = 1_000_000;
const C: u64 = 10_000_000;
fn now(elapsed: u64) -> ClientInstant {
    ClientInstant(C + elapsed)
}
fn grant() -> Granted {
    Granted {
        request: Request {
            parent: ControlBinding {
                id: 7,
                host_boot: HostBootId::from_raw(1),
                os_session: OsSessionId::from_raw(2),
                remote_session: RemoteSessionId::from_raw(3),
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
                bounds: InputBounds::new(DesktopPoint { x: 0, y: 0 }, 100, 100).unwrap(),
                capabilities: Capabilities::default().with(Capability::Keys),
            },
        },
        input_channel: 9,
        lease: InputLeaseId::from_raw(10),
        ticket: InputTicketId::from_raw(11),
        issued_at_us: H,
        lease_until_us: H + 3_000_000,
        ticket_until_us: H + 1_000_000,
        first_action: 0,
        first_pointer: 0,
    }
}
fn correlation(elapsed: u64) -> ClockCorrelation {
    ClockCorrelation::new(
        ClockSample {
            host_boot: grant().request.parent.host_boot,
            client_sent_us: C + elapsed,
            host_sample_us: H + elapsed,
            client_received_us: C + elapsed,
        },
        ClockPolicy {
            drift_ppm: 0,
            ..ClockPolicy::default()
        },
    )
    .unwrap()
}
fn selection() -> Selection {
    let mut capabilities: Vec<_> = [
        (fr_wire::audio_up::CAPABILITY, fr_wire::audio_up::VERSION),
        (
            fr_wire::attachment::CAPABILITY,
            fr_wire::attachment::VERSION,
        ),
        (
            fr_wire::attachment::INPUT_CAPABILITY,
            fr_wire::attachment::INPUT_VERSION,
        ),
    ]
    .into_iter()
    .map(|(name, version)| SelectedCapability {
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
fn profile() -> Profile {
    Profile {
        channels: AudioChannels::Mono,
        frame_duration_ms: 10,
        max_packet_bytes: 512,
        max_record_bytes: 1150,
        jitter_target_ms: 20,
    }
}
fn ready(input: &mut InputClient, elapsed: u64) {
    let g = grant();
    input
        .confirm_mapping(g.credentials().session, g.credentials().view, now(elapsed))
        .unwrap();
    input
        .presented(
            PresentedObservation {
                session: g.credentials().session,
                serial: elapsed,
                view: g.credentials().view,
                received_at: now(elapsed),
                source_age_upper_us: 0,
            },
            now(elapsed),
        )
        .unwrap();
}
fn accepted() -> InputClient {
    let g = grant();
    let mut request =
        RequestControl::new(g.request, g.input_channel, ProtocolLimits::ABSOLUTE, now(0)).unwrap();
    request.sent(now(0)).unwrap();
    let mut bytes = [0; control::GRANTED_BYTES];
    control::encode_granted(
        g,
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    let (_, mut input) = request
        .accept(
            &bytes,
            correlation(0),
            Policy {
                view_age_us: 1_500_000,
                ..Policy::default()
            },
            now(0),
        )
        .unwrap();
    ready(&mut input, 0);
    input
}
fn attached() -> (InputClient, Sender) {
    let mut input = accepted();
    let sender = input
        .attach_microphone(
            grant().request.parent,
            80,
            &selection(),
            true,
            profile(),
            now(0),
        )
        .unwrap();
    (input, sender)
}
fn ack(cfg: AudioConfiguration) -> Vec<u8> {
    let ack = audio::AudioConfigured {
        direction: cfg.direction,
        generation: cfg.generation,
        accepted: true,
        actual_channels: cfg.channels,
        actual_sample_rate: cfg.sample_rate,
        actual_frame_duration_ms: cfg.frame_duration_ms,
    };
    let mut bytes = vec![0; audio::AUDIO_CONFIGURED_RECORD_BYTES];
    audio::encode_configured(&ack, 80, &mut bytes).unwrap();
    bytes
}
fn start(sender: &mut Sender, elapsed: u64) -> CapturePermit {
    sender.set_permission(MicPermission::Granted);
    let cfg = sender
        .begin_talk(MicTalkMode::OpenMic { active: true }, now(elapsed))
        .unwrap();
    let encoded = sender.pending_configuration(now(elapsed)).unwrap().unwrap();
    assert_eq!(audio::decode_configuration(encoded, 80).unwrap(), cfg);
    sender.configuration_sent(now(elapsed)).unwrap();
    sender.configured(&ack(cfg), now(elapsed)).unwrap()
}

#[test]
fn local_panel_is_muted_until_an_explicit_talk_and_false_does_not_end_idle_control() {
    let (_input, mut sender) = attached();
    let requested = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let panel = requested.clone();
    sender.set_local_talk_check(Arc::new(move || {
        let token = panel.load(Ordering::Acquire);
        (token != 0).then_some(token)
    }));
    sender.set_permission(MicPermission::Granted);
    assert_eq!(sender.pending_configuration(now(1)), Ok(None));
    assert_eq!(sender.status(), Status::Muted);
    assert_eq!(
        sender.begin_talk(MicTalkMode::OpenMic { active: true }, now(2)),
        Err(Error::Permission)
    );
    assert_eq!(sender.pending_configuration(now(3)), Ok(None));
    requested.store(1, Ordering::Release);
    assert_eq!(sender.pending_configuration(now(4)), Ok(None));
    assert_eq!(
        sender.status(),
        Status::Muted,
        "permission alone cannot talk"
    );
    let _explicit = sender
        .begin_talk(MicTalkMode::OpenMic { active: true }, now(5))
        .unwrap();
    assert!(sender.pending_configuration(now(6)).unwrap().is_some());
}

#[test]
fn local_mute_fences_queued_configuration_without_waiting_for_the_session_task() {
    let (_input, mut sender) = attached();
    let requested = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let panel = requested.clone();
    sender.set_local_talk_check(Arc::new(move || {
        let token = panel.load(Ordering::Acquire);
        (token != 0).then_some(token)
    }));
    sender.set_permission(MicPermission::Granted);
    let original = sender
        .begin_talk(MicTalkMode::OpenMic { active: true }, now(0))
        .unwrap();
    let retained = sender.configuration_permit().unwrap();
    sender.configuration_sent(now(1)).unwrap();
    requested.store(0, Ordering::Release);
    // The native owner never samples the entire intervening mute interval.
    requested.store(2, Ordering::Release);
    assert!(
        !retained.valid(now(3)),
        "a new UI action cannot revive old bytes"
    );
    sender.mute();
    let replacement = sender
        .begin_talk(MicTalkMode::OpenMic { active: true }, now(4))
        .unwrap();
    assert!(replacement.generation > original.generation);
    assert!(sender.configuration_permit().unwrap().valid(now(5)));
    assert!(!retained.valid(now(5)));
}

#[test]
fn native_monitor_independently_fences_speech_on_ui_loss_and_never_revives_it() {
    let (_input, mut sender) = attached();
    let live = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let panel = live.clone();
    sender.set_local_talk_check(Arc::new(move || {
        let token = panel.load(Ordering::Acquire);
        (token != 0).then_some(token)
    }));
    let permit = start(&mut sender, 0);
    let monitor = permit.monitor();
    assert!(monitor.valid(now(1)));
    live.store(0, Ordering::Release);
    // No Sender operation runs between UI failure and the native monitor check.
    assert!(!monitor.valid(now(2)));
    assert!(!permit.valid(now(2)));
    assert!(sender.local_talk_withdrawn());
    live.store(2, Ordering::Release);
    assert!(!monitor.valid(now(3)));
    sender.mute();
    assert!(!sender.local_talk_withdrawn());
    let fresh = start(&mut sender, 4);
    assert!(fresh.valid(now(5)));
    assert!(!permit.valid(now(5)));
}

#[test]
fn authority_retirement_cannot_be_relabelled_as_a_later_local_mute() {
    let (input, mut sender) = attached();
    let token = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let local = token.clone();
    sender.set_local_talk_check(Arc::new(move || Some(local.load(Ordering::Acquire))));
    let permit = start(&mut sender, 0);
    drop(input);
    assert!(!permit.valid(now(1)));
    assert!(!sender.local_talk_withdrawn());
    token.store(2, Ordering::Release);
    assert!(!permit.valid(now(2)));
    let _ = sender.status();
    assert!(
        !sender.local_talk_withdrawn(),
        "the original authority fence wins"
    );
}

#[test]
fn idle_attachment_is_not_a_withdrawal_and_each_mute_retains_its_original_cause() {
    let (_input, mut sender) = attached();
    let token = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let local = token.clone();
    sender.set_local_talk_check(Arc::new(move || {
        let token = local.load(Ordering::Acquire);
        (token != 0).then_some(token)
    }));
    assert_eq!(sender.status(), Status::Muted);
    assert!(!sender.local_talk_withdrawn());
    token.store(1, Ordering::Release);
    let permit = start(&mut sender, 0);
    token.store(0, Ordering::Release);
    assert_eq!(sender.status(), Status::Muted);
    assert!(sender.local_talk_withdrawn());
    assert!(!permit.valid(now(1)));
    sender.mute();
    assert!(!sender.local_talk_withdrawn());
    token.store(2, Ordering::Release);
    let fresh = start(&mut sender, 2);
    assert!(fresh.valid(now(3)));
    assert!(!sender.local_talk_withdrawn());
    assert!(!permit.valid(now(3)));
}

#[test]
fn a_panicking_local_permission_check_cannot_start_recording() {
    let (_input, mut sender) = attached();
    sender.set_local_talk_check(Arc::new(|| panic!("local permission unavailable")));
    sender.set_permission(MicPermission::Granted);
    assert_eq!(
        sender.begin_talk(MicTalkMode::OpenMic { active: true }, now(0)),
        Err(Error::Permission)
    );
    assert_eq!(sender.status(), Status::Muted);
    assert!(sender.configuration_permit().is_none());
}

#[test]
fn configuration_queue_permit_observes_original_owner_without_capture_ack() {
    let (input, mut sender) = attached();
    assert!(sender.configuration_permit().is_none());
    sender.set_permission(MicPermission::Granted);
    sender
        .begin_talk(MicTalkMode::OpenMic { active: true }, now(0))
        .unwrap();
    let retained = sender.configuration_permit().unwrap();
    assert_eq!(
        retained.local_deadline(),
        sender.configuration_deadline().unwrap()
    );
    sender.configuration_sent(now(1)).unwrap();
    assert!(retained.valid(now(2)));
    drop(input);
    assert!(
        !retained.valid(now(3)),
        "queued configuration cannot outlive the original controller"
    );
}

#[test]
fn mute_and_permission_withdrawal_fence_retained_configuration_without_revival() {
    for permission_loss in [false, true] {
        let (_input, mut sender) = attached();
        sender.set_permission(MicPermission::Granted);
        let first = sender
            .begin_talk(MicTalkMode::OpenMic { active: true }, now(0))
            .unwrap();
        let retained = sender.configuration_permit().unwrap();
        sender.configuration_sent(now(1)).unwrap();
        if permission_loss {
            sender.set_permission(MicPermission::NotRequested);
        } else {
            sender.mute();
        }
        assert!(!retained.valid(now(2)));
        sender.set_permission(MicPermission::Granted);
        let next = sender
            .begin_talk(MicTalkMode::OpenMic { active: true }, now(3))
            .unwrap();
        assert!(next.generation > first.generation);
        assert!(sender.configuration_permit().unwrap().valid(now(4)));
        assert!(
            !retained.valid(now(4)),
            "fresh talk never revives old configuration custody"
        );
    }
}

#[test]
fn host_ack_does_not_extend_the_reliable_configuration_permit() {
    let (mut input, mut sender) = attached();
    sender.set_permission(MicPermission::Granted);
    let offer = sender
        .begin_talk(MicTalkMode::OpenMic { active: true }, now(0))
        .unwrap();
    let retained = sender.configuration_permit().unwrap();
    let until = retained.local_deadline();
    sender.configuration_sent(now(1)).unwrap();
    let capture = sender.configured(&ack(offer), now(2)).unwrap();
    assert!(
        sender.configuration_permit().is_none(),
        "an ACK cannot mint another config proof"
    );
    assert!(capture.local_deadline() > until);
    ready(&mut input, 1_000_000);
    ready(&mut input, 1_900_000);
    assert!(!retained.valid(until));
    assert!(
        capture.valid(until),
        "expiring old config custody cannot revoke newer capture authority"
    );
}

fn packet(permit: &CapturePermit, sequence: u64, timestamp: u64) -> AudioAccessUnit {
    AudioAccessUnit::new(
        AudioDirection::Uplink,
        permit.configuration().generation(),
        sequence,
        timestamp,
        480,
        false,
        &[1, 2, 3],
    )
    .unwrap()
}
#[test]
fn connection_capability_host_grant_and_os_grant_never_imply_talk() {
    let mut input = accepted();
    assert_eq!(
        input
            .attach_microphone(
                grant().request.parent,
                80,
                &selection(),
                false,
                profile(),
                now(0)
            )
            .unwrap_err(),
        Error::Permission
    );
    let mut observe = selection();
    observe.role = Role::Observe;
    assert_eq!(
        input
            .attach_microphone(
                grant().request.parent,
                80,
                &observe,
                true,
                profile(),
                now(0)
            )
            .unwrap_err(),
        Error::NotSelected
    );
    let mut missing = selection();
    missing
        .capabilities
        .retain(|c| c.name != fr_wire::audio_up::CAPABILITY);
    assert_eq!(
        input
            .attach_microphone(
                grant().request.parent,
                80,
                &missing,
                true,
                profile(),
                now(0)
            )
            .unwrap_err(),
        Error::NotSelected
    );
    let mut sender = input
        .attach_microphone(
            grant().request.parent,
            80,
            &selection(),
            true,
            profile(),
            now(0),
        )
        .unwrap();
    assert_eq!(sender.status(), Status::Muted);
    assert_eq!(
        sender.begin_talk(MicTalkMode::OpenMic { active: true }, now(0)),
        Err(Error::Permission)
    );
    sender.set_permission(MicPermission::Granted);
    assert_eq!(sender.status(), Status::Muted);
    assert!(sender.pending_configuration(now(0)).unwrap().is_none());
    assert_eq!(
        sender.begin_talk(MicTalkMode::Muted, now(0)),
        Err(Error::NotTalking)
    );
    assert_eq!(
        input
            .attach_microphone(
                grant().request.parent,
                81,
                &selection(),
                true,
                profile(),
                now(0)
            )
            .unwrap_err(),
        Error::Authority(crate::clipboard::Error::AlreadyAttached)
    );
}
#[test]
fn only_exact_positive_acknowledgement_creates_capture_permit() {
    for mismatch in 0..7 {
        let (_input, mut sender) = attached();
        sender.set_permission(MicPermission::Granted);
        let cfg = sender
            .begin_talk(MicTalkMode::PushToTalk { active: true }, now(0))
            .unwrap();
        sender.configuration_sent(now(0)).unwrap();
        let mut a = audio::AudioConfigured {
            direction: cfg.direction,
            generation: cfg.generation,
            accepted: true,
            actual_channels: cfg.channels,
            actual_sample_rate: cfg.sample_rate,
            actual_frame_duration_ms: cfg.frame_duration_ms,
        };
        let channel = match mismatch {
            0 => {
                a.accepted = false;
                80
            }
            1 => {
                a.direction = AudioDirection::Downlink;
                80
            }
            2 => {
                a.generation = AudioGeneration::from_raw(2);
                80
            }
            3 => {
                a.actual_channels = AudioChannels::Stereo;
                80
            }
            4 => {
                a.actual_frame_duration_ms = 20;
                80
            }
            5 => 81,
            _ => 80,
        };
        let mut bytes = [0; audio::AUDIO_CONFIGURED_RECORD_BYTES];
        audio::encode_configured(&a, channel, &mut bytes).unwrap();
        let result = sender.configured(&bytes, now(0));
        if mismatch == 6 {
            assert!(result.unwrap().valid(now(0)));
        } else {
            assert!(result.is_err());
            assert_eq!(sender.status(), Status::Stopped);
        }
    }
    let (_input, mut sender) = attached();
    sender.set_permission(MicPermission::Granted);
    let cfg = sender
        .begin_talk(MicTalkMode::OpenMic { active: true }, now(0))
        .unwrap();
    assert_eq!(
        sender.configured(&ack(cfg), now(0)).unwrap_err(),
        Error::Order
    );
}
#[test]
fn configuration_deadline_includes_backpressure_and_the_reply_wait() {
    let (mut input, mut sender) = attached();
    sender.set_permission(MicPermission::Granted);
    let cfg = sender
        .begin_talk(MicTalkMode::OpenMic { active: true }, now(0))
        .unwrap();
    let original = sender.configuration_deadline().unwrap();
    let bytes = sender
        .pending_configuration(now(0))
        .unwrap()
        .unwrap()
        .to_vec();
    ready(&mut input, 1_000_000);
    assert_eq!(
        sender
            .pending_configuration(now(1_000_000))
            .unwrap()
            .unwrap(),
        bytes
    );
    assert_eq!(sender.configuration_deadline(), Some(original));
    sender.configuration_sent(now(1_000_000)).unwrap();
    assert_eq!(
        sender.configured(&ack(cfg), original).unwrap_err(),
        Error::Expired
    );
    assert_eq!(sender.status(), Status::Muted);
}
#[test]
fn mute_permission_loss_and_device_change_invalidate_held_packets_and_old_epochs() {
    for transition in 0..4 {
        let (_input, mut sender) = attached();
        let permit = start(&mut sender, 0);
        let mut receipt = sender.begin_capture(&permit, now(1), now(100_001)).unwrap();
        sender
            .submit(&mut receipt, &packet(&permit, 0, 0), now(2))
            .unwrap();
        match transition {
            0 => {
                sender.mute();
            }
            1 => sender.set_permission(MicPermission::NotRequested),
            2 => {
                sender.device_changed();
            }
            _ => sender.set_permission(MicPermission::Denied),
        }
        assert!(!permit.valid(now(3)));
        assert!(!sender.has_pending_record());
        assert!(sender.pending_bytes.iter().all(|b| *b == 0));
        assert!(
            !sender
                .try_send(now(3), |_, _| panic!("muted packet escaped"))
                .unwrap()
        );
        let replacement = start(&mut sender, 4);
        assert!(
            replacement
                .configuration()
                .generation()
                .supersedes(permit.configuration().generation())
        );
        assert!(!permit.valid(now(4)));
        assert!(replacement.valid(now(4)));
        assert!(
            sender
                .submit(&mut receipt, &packet(&permit, 1, 480), now(4))
                .is_err()
        );
    }
}
#[test]
fn original_controller_drop_stop_and_view_suspension_fence_shared_permit_without_tick() {
    for transition in 0..3 {
        let (mut input, mut sender) = attached();
        let permit = Arc::new(start(&mut sender, 0));
        match transition {
            0 => drop(input),
            1 => input.stop(StopReason::Disconnected),
            _ => input.suspend(now(1)),
        }
        assert!(!permit.valid(now(2)));
        assert!(sender.begin_capture(&permit, now(2), now(100_002)).is_err());
    }
}
#[test]
fn optional_microphone_stop_does_not_revoke_clipboard_files_or_input() {
    let (mut input, mut sender) = attached();
    let mut clipboard = input.attach_clipboard(77, true, now(0)).unwrap();
    let files = input
        .file_receive_authority(grant().request.parent, 78, grant().lease, now(0))
        .unwrap();
    let permit = start(&mut sender, 0);
    sender.stop();
    assert!(!permit.valid(now(0)));
    assert_eq!(input.stopped(), None);
    assert!(files.sample(now(1)).is_ok());
    clipboard.local_switch().set_enabled(true);
    clipboard.peer_switch().set_enabled(true);
    assert!(clipboard.offer(1, "still live", None, now(1)).is_ok());
}
#[test]
fn packet_backpressure_preserves_the_original_hundred_millisecond_cap() {
    let (_input, mut sender) = attached();
    let permit = start(&mut sender, 0);
    assert_eq!(
        sender
            .begin_capture(&permit, now(1), now(100_002))
            .unwrap_err(),
        Error::Expired
    );
    let mut receipt = sender.begin_capture(&permit, now(1), now(100_001)).unwrap();
    receipt.restrict_expiry(now(50_001)).unwrap();
    sender
        .submit(&mut receipt, &packet(&permit, 0, 0), now(2))
        .unwrap();
    let mut observed = Vec::new();
    assert!(
        !sender
            .try_send(now(20_000), |bytes, until| {
                assert_eq!(until, now(50_001));
                observed = bytes.to_vec();
                Ok(false)
            })
            .unwrap()
    );
    assert!(sender.has_pending_record());
    assert_eq!(
        sender.try_send(now(50_001), |_, _| panic!("expired packet escaped")),
        Err(Error::Expired)
    );
    assert!(!sender.has_pending_record());
    assert!(!permit.valid(now(50_001)));
    assert_eq!(audio::decode_packet(&observed, 80).unwrap().sequence, 0);
}
#[test]
fn captured_metadata_cannot_change_scope_format_or_source_timeline() {
    for mismatch in 0..5 {
        let (_input, mut sender) = attached();
        let permit = start(&mut sender, 0);
        let mut receipt = sender.begin_capture(&permit, now(1), now(100_001)).unwrap();
        let direction = if mismatch == 0 {
            AudioDirection::Downlink
        } else {
            AudioDirection::Uplink
        };
        let generation = if mismatch == 1 {
            AudioGeneration::from_raw(99)
        } else {
            permit.configuration().generation()
        };
        let sequence = u64::from(mismatch == 2);
        let duration = if mismatch == 3 { 960 } else { 480 };
        let payload = vec![1; if mismatch == 4 { 513 } else { 3 }];
        let unit = AudioAccessUnit::new(
            direction, generation, sequence, 0, duration, false, &payload,
        )
        .unwrap();
        assert_eq!(
            sender.submit(&mut receipt, &unit, now(2)),
            Err(Error::InvalidPacket)
        );
        assert!(!permit.valid(now(2)));
    }
    let (_input, mut sender) = attached();
    let permit = start(&mut sender, 0);
    let mut receipt = sender.begin_capture(&permit, now(1), now(100_001)).unwrap();
    sender
        .submit(&mut receipt, &packet(&permit, 0, 123), now(2))
        .unwrap();
    assert!(sender.try_send(now(2), |_, _| Ok(true)).unwrap());
    assert_eq!(
        sender.submit(&mut receipt, &packet(&permit, 1, 604), now(3)),
        Err(Error::InvalidPacket)
    );
}
#[test]
fn one_bounded_batch_accepts_four_consecutive_packets_exactly_once() {
    let (_input, mut sender) = attached();
    let permit = start(&mut sender, 0);
    let mut receipt = sender.begin_capture(&permit, now(1), now(100_001)).unwrap();
    for sequence in 0..4 {
        sender
            .submit(
                &mut receipt,
                &packet(&permit, sequence, sequence * 480),
                now(2 + sequence),
            )
            .unwrap();
        assert!(
            sender
                .try_send(now(2 + sequence), |bytes, until| {
                    let p = audio::decode_packet(bytes, 80).unwrap();
                    assert_eq!(p.sequence, sequence);
                    assert_eq!(until, now(100_001));
                    Ok(true)
                })
                .unwrap()
        );
        assert!(
            !sender
                .try_send(now(2 + sequence), |_, _| panic!("duplicate send"))
                .unwrap()
        );
    }
    assert_eq!(
        sender.submit(&mut receipt, &packet(&permit, 4, 1920), now(6)),
        Err(Error::InvalidPacket)
    );
}
#[test]
fn worker_permission_is_send_sync_and_concurrent_clock_overtaking_is_conservative() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CapturePermit>();
    let (_input, mut sender) = attached();
    let permit = Arc::new(start(&mut sender, 0));
    let worker = permit.clone();
    std::thread::spawn(move || assert!(worker.valid(now(20))))
        .join()
        .unwrap();
    assert!(permit.valid(now(10))); // older sample may acquire the mutex later
    assert!(sender.begin_capture(&permit, now(21), now(100_021)).is_ok());
    assert_eq!(
        sender
            .begin_capture(&permit, now(20), now(100_020))
            .unwrap_err(),
        Error::Clock
    );
    assert!(!permit.valid(now(21)));
}
#[test]
fn overtaken_capture_timestamp_cannot_admit_expired_speech() {
    let (_input, mut sender) = attached();
    let permit = start(&mut sender, 0);
    let mut receipt = sender.begin_capture(&permit, now(1), now(100_001)).unwrap();
    // The original worker has already serviced a later clock sample while
    // the application callback retained an earlier, otherwise ordered sample.
    assert!(permit.valid(now(100_001)));
    assert_eq!(
        sender.submit(&mut receipt, &packet(&permit, 0, 0), now(100_000)),
        Err(Error::Expired)
    );
    assert!(!sender.has_pending_record());
    assert!(!permit.valid(now(100_001)));
}

#[test]
fn overtaken_send_timestamp_never_exposes_expired_speech_to_transport() {
    let (_input, mut sender) = attached();
    let permit = start(&mut sender, 0);
    let mut receipt = sender.begin_capture(&permit, now(1), now(100_001)).unwrap();
    sender
        .submit(&mut receipt, &packet(&permit, 0, 0), now(2))
        .unwrap();
    assert!(permit.valid(now(100_001)));
    let mut called = false;
    let result = sender.try_send(now(100_000), |_, _| {
        called = true;
        Ok(true)
    });
    assert!(
        !called,
        "known-expired speech reached the transport callback"
    );
    assert_eq!(result, Err(Error::Expired));
    assert!(!sender.has_pending_record());
    assert!(!permit.valid(now(100_001)));
}

#[test]
fn overtaken_capture_request_cannot_open_expired_custody() {
    let (_input, mut sender) = attached();
    let permit = start(&mut sender, 0);
    assert!(permit.valid(now(100_001)));
    assert_eq!(
        sender
            .begin_capture(&permit, now(100_000), now(100_001))
            .unwrap_err(),
        Error::Expired
    );
    // No native pull or captured work existed; rejecting this old request
    // does not turn it into a new lifetime or consume the live talk epoch.
    assert!(permit.valid(now(100_001)));
}

#[test]
fn overtaken_configuration_timestamp_cannot_restart_expired_talk_work() {
    for started in [false, true] {
        let (mut input, mut sender) = attached();
        sender.set_permission(MicPermission::Granted);
        let configuration = started.then(|| {
            let cfg = sender
                .begin_talk(MicTalkMode::OpenMic { active: true }, now(0))
                .unwrap();
            sender.configuration_sent(now(0)).unwrap();
            cfg
        });
        ready(&mut input, 1_000_000);
        ready(&mut input, 2_000_000);
        if let Some(cfg) = configuration {
            assert_eq!(
                sender.configured(&ack(cfg), now(1_999_999)).unwrap_err(),
                Error::Expired
            );
        } else {
            assert_eq!(
                sender.begin_talk(MicTalkMode::OpenMic { active: true }, now(0)),
                Err(Error::Expired)
            );
        }
        assert_eq!(sender.status(), Status::Muted);
        assert_eq!(sender.configuration_deadline(), None);
    }
}

#[test]
fn overtaken_send_callback_expires_only_retained_capture_work() {
    for admitted in [false, true] {
        let (_input, mut sender) = attached();
        let permit = start(&mut sender, 0);
        let mut receipt = sender.begin_capture(&permit, now(1), now(100_001)).unwrap();
        sender
            .submit(&mut receipt, &packet(&permit, 0, 0), now(2))
            .unwrap();
        let mut called = false;
        let result = sender.try_send(now(3), |_, until| {
            assert_eq!(until, now(100_001));
            called = true;
            assert!(permit.valid(until));
            Ok(admitted)
        });
        assert!(called);
        assert_eq!(
            result,
            if admitted {
                Ok(true)
            } else {
                Err(Error::Expired)
            }
        );
        assert!(!sender.has_pending_record());
        assert!(sender.pending_bytes.iter().all(|byte| *byte == 0));
        assert_eq!(permit.valid(now(100_001)), admitted);
        if admitted {
            // The original handoff remains accepted, but no further packet
            // may borrow that capture receipt after its immutable deadline.
            assert_eq!(
                sender.submit(&mut receipt, &packet(&permit, 1, 480), now(100_001)),
                Err(Error::Expired)
            );
        }
    }
}

#[test]
fn overtaken_live_samples_keep_original_custody_and_owner_clock_order() {
    let (_input, mut sender) = attached();
    let permit = start(&mut sender, 0);
    let mut receipt = sender.begin_capture(&permit, now(1), now(100_001)).unwrap();
    assert!(permit.valid(now(100_000)));
    sender
        .submit(&mut receipt, &packet(&permit, 0, 0), now(2))
        .unwrap();
    assert!(
        sender
            .try_send(now(3), |_, until| {
                assert_eq!(until, now(100_001));
                Ok(true)
            })
            .unwrap()
    );
    let next = sender.begin_capture(&permit, now(4), now(100_004)).unwrap();
    assert_eq!(next.acquired(), now(4));
    assert_eq!(next.expires(), now(100_004));
}

#[test]
fn callback_failure_or_caught_panic_fences_worker_and_erases_retained_speech() {
    for panic in [false, true] {
        let (_input, mut sender) = attached();
        let permit = start(&mut sender, 0);
        let mut receipt = sender.begin_capture(&permit, now(1), now(100_001)).unwrap();
        sender
            .submit(&mut receipt, &packet(&permit, 0, 0), now(2))
            .unwrap();
        if panic {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sender.try_send(now(3), |_, _| panic!("fixture transport"))
            }));
            assert!(result.is_err());
        } else {
            assert_eq!(
                sender.try_send(now(3), |_, _| Err(())),
                Err(Error::Transport)
            );
        }
        assert_eq!(sender.status(), Status::Stopped);
        assert!(!permit.valid(now(3)));
        assert!(sender.pending_bytes.iter().all(|b| *b == 0));
    }
}
#[test]
fn renewal_and_equal_ids_cannot_replace_the_original_authorization_cap() {
    let (mut input, mut sender) = attached();
    let permit = start(&mut sender, 0);
    let until = permit.local_deadline();
    let mut ticket = grant().initial_ticket();
    ticket.sequence = 1;
    ticket.credentials.ticket = InputTicketId::from_raw(12);
    ticket.issued_at_us = H + 2_000_000;
    ticket.expires_at_us = H + 2_500_000;
    ready(&mut input, 1_000_000);
    let mut bytes = [0; fr_wire::input_ticket::AUTHORITY_TICKET_BYTES];
    fr_wire::input_ticket::encode_authority(
        fr_wire::input_ticket::AuthorityTicket {
            ticket,
            authority_until_us: H + 5_000_000,
        },
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        9,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    input
        .accept_authority_ticket(&bytes, correlation(2_000_000), now(2_000_000))
        .unwrap();
    ready(&mut input, 2_000_000);
    assert_eq!(permit.local_deadline(), until);
    assert!(!permit.valid(until));
    drop(input);
    let _same_numbers = accepted();
    assert!(!permit.valid(until));
}

#[test]
fn a_tighter_clock_model_cannot_extend_the_rolling_authority_cap_of_new_capture() {
    let (mut input, mut sender) = attached();
    let old = start(&mut sender, 0);
    assert!(old.valid(now(1_000_000)));
    sender.mute();
    let mut ticket = grant().initial_ticket();
    ticket.sequence = 1;
    ticket.credentials.ticket = InputTicketId::from_raw(12);
    ticket.issued_at_us = H + 500_000;
    ticket.expires_at_us = H + 1_500_000;
    let mut bytes = [0; fr_wire::input_ticket::AUTHORITY_TICKET_BYTES];
    fr_wire::input_ticket::encode_authority(
        fr_wire::input_ticket::AuthorityTicket {
            ticket,
            authority_until_us: H + 3_500_000,
        },
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        9,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    let tighter = ClockCorrelation::new(
        ClockSample {
            host_boot: grant().request.parent.host_boot,
            client_sent_us: C + 1_000_000,
            host_sample_us: H + 800_000,
            client_received_us: C + 1_000_000,
        },
        ClockPolicy {
            drift_ppm: 0,
            ..ClockPolicy::default()
        },
    )
    .unwrap();
    input
        .accept_authority_ticket(&bytes, tighter, now(1_000_000))
        .unwrap();
    ready(&mut input, 1_000_000);
    let replacement = start(&mut sender, 1_000_000);
    assert_eq!(replacement.local_deadline(), now(3_500_000));
    assert!(!old.valid(now(1_000_001)));
    // A sample captured just before the worker's cursor update is harmless.
    assert!(replacement.valid(now(1_000_100)));
    assert_eq!(
        sender.clock.local_deadline(now(1_000_050)).unwrap(),
        now(3_500_000)
    );
}

#[test]
fn an_overtaken_sample_cannot_validate_a_fixed_permit_after_controller_renewal() {
    let (mut input, mut sender) = attached();
    let original = start(&mut sender, 0);
    assert_eq!(original.local_deadline(), now(3_000_000));
    ready(&mut input, 1_000_000);
    let mut ticket = grant().initial_ticket();
    ticket.sequence = 1;
    ticket.credentials.ticket = InputTicketId::from_raw(12);
    ticket.issued_at_us = H + 2_000_000;
    ticket.expires_at_us = H + 2_500_000;
    let mut bytes = [0; fr_wire::input_ticket::AUTHORITY_TICKET_BYTES];
    fr_wire::input_ticket::encode_authority(
        fr_wire::input_ticket::AuthorityTicket {
            ticket,
            authority_until_us: H + 5_000_000,
        },
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        9,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    input
        .accept_authority_ticket(&bytes, correlation(2_000_000), now(2_000_000))
        .unwrap();
    ready(&mut input, 2_000_000);
    // This sample was taken before expiry, but the worker is not scheduled
    // until the original controller has serviced a later monotonic instant.
    let delayed_sample = now(2_999_999);
    ready(&mut input, 3_000_001);
    assert_eq!(original.check(delayed_sample), Err(Error::Expired));
    assert!(!original.valid(delayed_sample));
    // The fixed old permit ends, not the separately renewed controller.
    sender.mute();
    let replacement = start(&mut sender, 3_000_002);
    assert_eq!(replacement.local_deadline(), now(5_000_000));
    assert!(replacement.valid(now(3_000_002)));
}

fn renewed_input(input: &mut InputClient, sequence: u64, elapsed: u64, until: u64) {
    let mut ticket = grant().initial_ticket();
    ticket.sequence = sequence;
    ticket.credentials.ticket = InputTicketId::from_raw(100 + u128::from(sequence));
    ticket.issued_at_us = H + elapsed;
    ticket.expires_at_us = H + elapsed + 500_000;
    let mut bytes = [0; fr_wire::input_ticket::AUTHORITY_TICKET_BYTES];
    fr_wire::input_ticket::encode_authority(
        fr_wire::input_ticket::AuthorityTicket {
            ticket,
            authority_until_us: H + until,
        },
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        9,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    input
        .accept_authority_ticket(&bytes, correlation(elapsed), now(elapsed))
        .unwrap();
    ready(input, elapsed);
}

#[test]
fn explicit_successor_keeps_old_capture_proofs_fixed_and_same_epoch_live() {
    let (mut input, mut sender) = attached();
    let original = start(&mut sender, 0);
    let monitor = original.monitor();
    ready(&mut input, 1_000_000);
    renewed_input(&mut input, 1, 2_000_000, 5_000_000);
    let successor = sender
        .refresh_capture(&original, now(2_000_000))
        .unwrap()
        .unwrap();
    assert_eq!(successor.configuration(), original.configuration());
    assert_eq!(original.local_deadline(), now(3_000_000));
    assert_eq!(successor.local_deadline(), now(5_000_000));
    assert_eq!(original.check(now(3_000_000)), Err(Error::Expired));
    assert!(successor.valid(now(3_000_000)));
    assert!(monitor.valid(now(3_000_000)));
    let receipt = sender
        .begin_capture(&successor, now(3_000_001), now(3_100_001))
        .unwrap();
    assert_eq!(receipt.expires(), now(3_100_001));
    sender.mute();
    assert!(!successor.valid(now(3_000_002)));
    assert!(!monitor.valid(now(3_000_002)));
}

#[test]
fn successor_cannot_extend_backpressured_packet_custody() {
    let (mut input, mut sender) = attached();
    let original = start(&mut sender, 0);
    ready(&mut input, 1_000_000);
    renewed_input(&mut input, 1, 2_000_000, 5_000_000);
    let mut receipt = sender
        .begin_capture(&original, now(2_000_000), now(2_050_000))
        .unwrap();
    let successor = sender
        .refresh_capture(&original, now(2_000_001))
        .unwrap()
        .unwrap();
    assert_eq!(receipt.expires(), now(2_050_000));
    sender
        .submit(&mut receipt, &packet(&original, 0, 0), now(2_000_002))
        .unwrap();
    assert!(
        !sender
            .try_send(now(2_000_003), |_, until| {
                assert_eq!(until, now(2_050_000));
                Ok(false)
            })
            .unwrap()
    );
    assert_eq!(
        sender.try_send(now(2_050_000), |_, _| panic!("expired speech escaped")),
        Err(Error::Expired)
    );
    assert!(!successor.valid(now(2_050_000)));
    assert_eq!(input.stopped(), None);
}

#[test]
fn late_or_foreign_refresh_cannot_revive_capture_authority() {
    let (mut input, mut sender) = attached();
    let original = start(&mut sender, 0);
    let (_other_input, mut other_sender) = attached();
    let foreign = start(&mut other_sender, 0);
    assert_eq!(
        sender.refresh_capture(&foreign, now(1)).unwrap_err(),
        Error::WrongScope
    );
    assert!(original.valid(now(1)));
    ready(&mut input, 1_000_000);
    renewed_input(&mut input, 1, 2_000_000, 5_000_000);
    assert_eq!(
        sender
            .refresh_capture(&original, now(3_000_000))
            .unwrap_err(),
        Error::Expired
    );
    assert!(!original.monitor().valid(now(3_000_000)));
    assert!(sender.refresh_capture(&original, now(3_000_001)).is_err());
    assert_eq!(input.stopped(), None);
}

#[test]
fn refresh_requires_useful_fresh_proof_and_current_predecessor() {
    let (mut input, mut sender) = attached();
    let original = start(&mut sender, 0);
    renewed_input(&mut input, 1, 1_000_000, 4_000_000);
    assert!(
        sender
            .refresh_capture(&original, now(1_000_000))
            .unwrap()
            .is_none()
    );
    ready(&mut input, 2_000_000);
    let next = sender
        .refresh_capture(&original, now(2_000_000))
        .unwrap()
        .unwrap();
    assert_eq!(
        sender
            .refresh_capture(&original, now(2_000_001))
            .unwrap_err(),
        Error::Order
    );
    assert!(
        sender
            .refresh_capture(&next, now(2_000_002))
            .unwrap()
            .is_none()
    );
    assert!(next.valid(now(2_000_002)));
}

#[test]
fn a_shorter_authenticated_cap_fences_all_original_monitors_without_waiting_for_refresh_rate() {
    let (mut input, mut sender) = attached();
    let original = start(&mut sender, 0);
    let monitor = original.monitor();
    ready(&mut input, 1_000_000);
    renewed_input(&mut input, 1, 2_000_000, 2_500_000);
    let shorter = sender
        .refresh_capture(&original, now(2_000_000))
        .unwrap()
        .unwrap();
    assert_eq!(shorter.local_deadline(), now(2_500_000));
    assert_eq!(original.local_deadline(), now(3_000_000));
    assert!(monitor.valid(now(2_499_999)));
    assert_eq!(original.check(now(2_500_000)), Err(Error::Expired));
    assert!(!shorter.valid(now(2_500_000)));
    assert!(!monitor.valid(now(2_500_000)));
}

#[test]
fn capture_renewal_coalesces_tiny_advances_and_bounds_control_exchange_rate() {
    let (mut input, mut sender) = attached();
    let original = start(&mut sender, 0);
    ready(&mut input, 1_000_000);
    renewed_input(&mut input, 1, 2_000_000, 3_000_001);
    assert!(
        sender
            .refresh_capture(&original, now(2_000_000))
            .unwrap()
            .is_none()
    );
    renewed_input(&mut input, 2, 2_010_000, 3_150_000);
    let next = sender
        .refresh_capture(&original, now(2_010_000))
        .unwrap()
        .unwrap();
    renewed_input(&mut input, 3, 2_020_000, 3_350_000);
    assert!(
        sender
            .refresh_capture(&next, now(2_020_000))
            .unwrap()
            .is_none()
    );
    let third = sender
        .refresh_capture(&next, now(2_260_000))
        .unwrap()
        .unwrap();
    assert_eq!(third.local_deadline(), now(3_350_000));
    assert_eq!(next.local_deadline(), now(3_150_000));
}
