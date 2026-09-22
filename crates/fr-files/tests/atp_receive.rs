#![cfg(target_os = "linux")]
use asupersync::{
    atp::object::ContentId,
    bytes::BytesMut,
    codec::Decoder,
    cx::Cx,
    net::atp::{
        protocol::{
            codec::AtpFrameCodec,
            frames::{Frame, FrameType, ProtocolVersion},
        },
        transport_common::{StagedEntryReceive, flat_merkle_root_from_digests, hex_encode},
        transport_tcp::{ManifestEntry, ReceiveReceipt, TransferManifest},
    },
    runtime::{Runtime, RuntimeBuilder},
    time::{TimerDriverHandle, VirtualClock},
    types::{Budget, CancelKind},
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_submission::{Capabilities, InputSession},
    time::HostInstant,
};
use fr_files::session::Permission;
use fr_files::{
    atp::receive::{Error, Event, MAX_FRAME_BYTES, MAX_REPLY_BYTES, Receiver},
    receive::{self, DropDirectory, Limits, Publication},
    session::{self, Policy},
    worker::{self, Completion, Task},
};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
fn owner() -> InputSession {
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
    let now = HostInstant::from_micros(0);
    let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(now).unwrap();
    a.mark_view_ready(now).unwrap();
    a.grant_lease(c.lease, now).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, now).unwrap();
    InputSession::new(
        a,
        c,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        now,
    )
    .unwrap()
}
struct Fixture {
    root: PathBuf,
    directory: DropDirectory,
    clock: Arc<VirtualClock>,
    cx: Cx,
    input: Option<InputSession>,
    _runtime: Runtime,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "fr-file-atp-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let directory = DropDirectory::open(
            &root,
            Limits {
                max_file_bytes: 1024 * 1024,
                max_reserved_bytes: 1024 * 1024,
                max_transfers: 1,
            },
        )
        .unwrap();
        let clock = Arc::new(VirtualClock::new());
        let runtime = RuntimeBuilder::new()
            .worker_threads(1)
            .with_timer_driver(TimerDriverHandle::with_virtual_clock(clock.clone()))
            .build()
            .unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        Self {
            root,
            directory,
            clock,
            cx,
            input: Some(owner()),
            _runtime: runtime,
        }
    }
    fn spawn(&self, policy: Policy, frame_bytes: usize) -> (Receiver, Task) {
        Receiver::spawn(
            self.cx.clone(),
            self.input.as_ref().unwrap(),
            self.directory.clone(),
            Permission::new(true),
            policy,
            frame_bytes,
        )
        .unwrap()
    }
    fn empty(&self) {
        assert_eq!(fs::read_dir(&self.root).unwrap().count(), 0);
    }
    fn input_live(&self) {
        assert!(
            self.input
                .as_ref()
                .unwrap()
                .monitor()
                .deadline(HostInstant::from_micros(0))
                .is_ok()
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn wait(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !predicate() {
        assert!(Instant::now() < deadline, "bounded worker wait expired");
        thread::sleep(Duration::from_millis(1));
    }
}
fn wire(kind: FrameType, payload: Vec<u8>) -> Vec<u8> {
    Frame::new(ProtocolVersion::CURRENT, kind, payload)
        .unwrap()
        .to_wire_bytes()
        .unwrap()
}
fn parse(bytes: &[u8]) -> Frame {
    let mut bytes = BytesMut::from(bytes);
    let frame = AtpFrameCodec::with_max_frame_size(MAX_FRAME_BYTES as u64)
        .decode(&mut bytes)
        .unwrap()
        .unwrap();
    assert!(bytes.is_empty());
    frame
}
fn manifest(name: &str, bytes: &[u8]) -> TransferManifest {
    let mut digest = StagedEntryReceive::new(PathBuf::from("test-streaming-hash-only"));
    digest.update_with_chunk(bytes);
    let (digest, _, _) = digest.finalize(name.into());
    TransferManifest {
        // Stable ATP label is metadata, NOT a controller/attachment credential.
        transfer_id: "0123456789abcdef0123456789abcdef".into(),
        root_name: name.into(),
        is_directory: false,
        total_bytes: bytes.len() as u64,
        merkle_root_hex: flat_merkle_root_from_digests(std::slice::from_ref(&digest)),
        metadata_root_hex: None,
        directory_metadata: None,
        delta_manifest: None,
        entries: vec![ManifestEntry {
            index: 0,
            rel_path: name.into(),
            size: bytes.len() as u64,
            sha256_hex: hex_encode(&digest.content_sha256),
            metadata: None,
            members: vec![],
        }],
    }
}
fn offered(offering: &TransferManifest) -> Vec<u8> {
    wire(
        FrameType::ObjectManifest,
        serde_json::to_vec(offering).unwrap(),
    )
}
fn data(index: u32, offset: u64, bytes: &[u8]) -> Vec<u8> {
    let mut payload = index.to_be_bytes().to_vec();
    payload.extend_from_slice(&offset.to_be_bytes());
    payload.extend_from_slice(bytes);
    wire(FrameType::ObjectData, payload)
}
fn push(receiver: &mut Receiver, id: u64, frame: &[u8]) -> u64 {
    let mut result = None;
    wait(|| match receiver.push(receiver.binding(), id, frame) {
        Ok(sequence) => {
            result = Some(sequence);
            true
        }
        Err(Error::Busy | Error::Worker(worker::Error::Busy)) => false,
        Err(error) => panic!("frame refused: {error:?}"),
    });
    result.unwrap()
}
fn event(receiver: &mut Receiver) -> Event {
    let mut event = None;
    wait(|| match receiver.poll() {
        Ok(Some(value)) => {
            event = Some(value);
            true
        }
        Ok(None) | Err(Error::Worker(worker::Error::Busy)) => false,
        Err(error) => panic!("poll failed: {error:?}"),
    });
    event.unwrap()
}
fn stop(task: &mut Task) {
    task.stop();
    wait(|| task.is_finished());
    let _ = task.try_finish().unwrap();
}
fn begin(receiver: &mut Receiver, id: u64, offering: &TransferManifest) -> Event {
    push(receiver, id, &offered(offering));
    let outcome = event(receiver);
    assert!(matches!(outcome.outcome(), Ok(Completion::Begun(_))));
    outcome
}
fn stage(receiver: &mut Receiver, bytes: &[u8]) {
    begin(receiver, 1, &manifest("received.bin", bytes));
    if !bytes.is_empty() {
        push(receiver, 1, &data(0, 0, bytes));
        assert!(matches!(
            event(receiver).outcome(),
            Ok(Completion::Written(_))
        ));
    }
}

#[test]
fn pinned_atp_manifest_data_and_proof_publish_real_multiframe_file() {
    let fixture = Fixture::new();
    let (mut receiver, mut task) = fixture.spawn(Policy::conservative(), 8192);
    let bytes: Vec<u8> = (0..200_007)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    let offering = manifest("received.bin", &bytes);
    let accepted = begin(&mut receiver, 1, &offering);
    let mut reply = [0; MAX_REPLY_BYTES];
    let length = accepted.encode_reply(&mut reply).unwrap().unwrap();
    let frame = parse(&reply[..length]);
    assert_eq!(frame.frame_type(), FrameType::ObjectRequest);
    let request: serde_json::Value = serde_json::from_slice(&frame.payload).unwrap();
    assert_eq!(request["mode"], "full_object");
    assert_eq!(request["sender_merkle_root_hex"], offering.merkle_root_hex);
    assert_eq!(request["missing_bytes"], bytes.len());
    let mut offset = 0;
    for chunk in bytes.chunks(7000) {
        fixture.clock.advance(20_000_000);
        push(&mut receiver, 1, &data(0, offset, chunk));
        assert_eq!(
            receiver.push(
                receiver.binding(),
                1,
                &wire(FrameType::ObjectComplete, vec![])
            ),
            Err(Error::Busy)
        );
        let outcome = event(&mut receiver);
        let Ok(Completion::Written(p)) = outcome.outcome() else {
            panic!("write failed")
        };
        offset += chunk.len() as u64;
        assert_eq!(p.staged_bytes, offset);
        assert!(outcome.encode_reply(&mut reply).unwrap().is_none());
        assert!(!fixture.root.join("received.bin").exists());
    }
    push(&mut receiver, 1, &wire(FrameType::ObjectComplete, vec![]));
    let outcome = event(&mut receiver);
    let Ok(Completion::Published(result)) = outcome.outcome() else {
        panic!("not published")
    };
    assert_eq!(result.publication, Publication::Durable);
    assert_eq!(fs::read(fixture.root.join("received.bin")).unwrap(), bytes);
    let length = outcome.encode_reply(&mut reply).unwrap().unwrap();
    let frame = parse(&reply[..length]);
    assert_eq!(frame.frame_type(), FrameType::Proof);
    let proof: ReceiveReceipt = serde_json::from_slice(&frame.payload).unwrap();
    assert!(proof.committed && proof.sha_ok && proof.merkle_ok);
    assert_eq!(proof.bytes_received, bytes.len() as u64);
    assert_eq!(proof.files, 1);
    assert_eq!(proof.committed_paths.len(), 0);
    assert!(proof.reason.is_none());
    assert!(!format!("{receiver:?} {outcome:?}").contains("received.bin"));
    assert!(!String::from_utf8_lossy(&reply[..length]).contains(fixture.root.to_str().unwrap()));
    stop(&mut task);
}

#[test]
fn empty_object_still_requires_manifest_merkle_verification_and_completion() {
    let fixture = Fixture::new();
    let (mut receiver, mut task) = fixture.spawn(Policy::conservative(), 1024);
    stage(&mut receiver, b"");
    assert!(!fixture.root.join("received.bin").exists());
    push(&mut receiver, 1, &wire(FrameType::ObjectComplete, vec![]));
    assert!(matches!(
        event(&mut receiver).outcome(),
        Ok(Completion::Published(_))
    ));
    assert_eq!(
        fs::metadata(fixture.root.join("received.bin"))
            .unwrap()
            .len(),
        0
    );
    stop(&mut task);
}

#[test]
fn plain_sha_content_id_and_merkle_are_distinct_fail_closed_contracts() {
    for corrupt in 0..3 {
        let fixture = Fixture::new();
        let (mut receiver, mut task) = fixture.spawn(Policy::conservative(), 4096);
        let mut offering = manifest("received.bin", b"abc");
        if corrupt == 0 {
            offering.entries[0].sha256_hex = ContentId::from_bytes(b"abc").to_hex();
        }
        if corrupt == 1 {
            offering.merkle_root_hex = "00".repeat(32);
        }
        begin(&mut receiver, 1, &offering);
        push(
            &mut receiver,
            1,
            &data(0, 0, if corrupt == 2 { b"abd" } else { b"abc" }),
        );
        event(&mut receiver);
        push(&mut receiver, 1, &wire(FrameType::ObjectComplete, vec![]));
        let outcome = event(&mut receiver);
        assert_eq!(
            outcome.outcome(),
            Err(worker::Error::Session(session::Error::Storage(
                receive::Error::Integrity
            )))
        );
        assert_eq!(
            outcome.encode_reply(&mut [0; MAX_REPLY_BYTES]).unwrap(),
            None
        );
        stop(&mut task);
        fixture.empty();
    }
}

#[test]
fn unsupported_and_malformed_manifest_cannot_create_any_file() {
    let base = serde_json::to_value(manifest("received.bin", b"abc")).unwrap();
    for case in 0..15 {
        let fixture = Fixture::new();
        let (mut receiver, mut task) = fixture.spawn(Policy::conservative(), 4096);
        let mut value = base.clone();
        #[rustfmt::skip]
        match case {
            0 => value["is_directory"] = true.into(),
            1 => value["entries"] = serde_json::json!([]),
            2 => value["entries"] = serde_json::json!([base["entries"][0], base["entries"][0]]),
            3 => value["root_name"] = "../escape".into(),
            4 => { value["root_name"] = "../escape".into(); value["entries"][0]["rel_path"] = "../escape".into(); }
            5 => value["total_bytes"] = 4.into(),
            6 => value["entries"][0]["index"] = 1.into(),
            7 => value["entries"][0]["sha256_hex"] = "not a hash".into(),
            8 => value["merkle_root_hex"] = "FF".repeat(32).into(),
            9 => value["transfer_id"] = "secret string not authority".into(),
            10 => value["metadata_root_hex"] = "00".repeat(32).into(),
            11 => value["unknown"] = "must refuse".into(),
            12 => { value["root_name"] = ".fr-part-private".into(); value["entries"][0]["rel_path"] = ".fr-part-private".into(); }
            13 => { value["root_name"] = "COM1".into(); value["entries"][0]["rel_path"] = "COM1".into(); }
            14 => value["entries"][0]["members"] = serde_json::json!([{"rel_path":"packed","offset":0,"len":3,"sha256_hex":"00".repeat(32)}]),
            _ => unreachable!(),
        }
        let manifest_payload = wire(FrameType::ObjectManifest, serde_json::to_vec(&value).unwrap());
        assert!(receiver.push(receiver.binding(), 1, &manifest_payload).is_err(), "case {case}");
        assert!(receiver.is_closed());
        stop(&mut task);
        fixture.empty();
        fixture.input_live();
    }
}

#[test]
fn wrong_offsets_indexes_empty_chunks_and_early_completion_retire_only_files() {
    #[rustfmt::skip]
    let bad_cases = [data(1, 0, b"abc"), data(0, 1, b"abc"), data(0, 0, b"abcd"), data(0, u64::MAX, b"abc"), data(0, 0, b""), wire(FrameType::ObjectComplete, vec![]), wire(FrameType::ObjectComplete, vec![1])];
    for bad in bad_cases {
        let fixture = Fixture::new();
        let (mut receiver, mut task) = fixture.spawn(Policy::conservative(), 4096);
        begin(&mut receiver, 1, &manifest("received.bin", b"abc"));
        assert!(receiver.push(receiver.binding(), 1, &bad).is_err());
        assert!(receiver.is_closed());
        stop(&mut task);
        fixture.empty();
        fixture.input_live();
    }
}

#[test]
fn complete_frame_bounds_and_disallowed_atp_messages_fail_before_disk() {
    let good = offered(&manifest("received.bin", b"abc"));
    let mut multiple = good.clone();
    multiple.extend_from_slice(&good);
    let mut extended = Frame::new(
        ProtocolVersion::CURRENT,
        FrameType::ObjectManifest,
        b"{}".to_vec(),
    )
    .unwrap();
    extended
        .header
        .extensions
        .insert(1, b"unnegotiated".to_vec());
    #[rustfmt::skip]
    let cases = [
        good[..good.len() - 1].to_vec(), multiple, extended.to_wire_bytes().unwrap(),
        wire(FrameType::Handshake, b"{\"peer_id\":\"host\"}".to_vec()), wire(FrameType::ObjectRequest, b"{}".to_vec()),
        wire(FrameType::Proof, b"{}".to_vec()), wire(FrameType::PathUpdate, vec![]), wire(FrameType::Repair, vec![]),
        vec![0; 4097], vec![0xff; 100],
    ];
    for bad in cases {
        let fixture = Fixture::new();
        let (mut receiver, mut task) = fixture.spawn(Policy::conservative(), 4096);
        assert!(receiver.push(receiver.binding(), 1, &bad).is_err());
        stop(&mut task);
        fixture.empty();
    }
}

#[test]
fn old_binding_transfer_and_cancel_cannot_redirect_or_terminate_new_transfer() {
    let fixture = Fixture::new();
    let (mut receiver, mut task) = fixture.spawn(Policy::conservative(), 4096);
    let original = receiver.binding();
    let mut stale = original;
    stale.lease = InputLeaseId::from_raw(999);
    assert_eq!(
        receiver.push(stale, 1, &offered(&manifest("received.bin", b"abc"))),
        Err(Error::WrongBinding)
    );
    begin(&mut receiver, 2, &manifest("received.bin", b"abc"));
    assert_eq!(
        receiver.push(original, 1, &data(0, 0, b"abc")),
        Err(Error::WrongTransfer)
    );
    assert_eq!(receiver.cancel(original, 1), Err(Error::WrongTransfer));
    assert_eq!(receiver.cancel(stale, 2), Err(Error::WrongBinding));
    assert!(!receiver.is_closed());
    receiver.cancel(original, 2).unwrap();
    stop(&mut task);
    fixture.empty();
    fixture.input_live();
}

#[test]
fn publication_result_survives_cancellation_and_failed_reply_encoding() {
    let fixture = Fixture::new();
    let (mut receiver, mut task) = fixture.spawn(Policy::conservative(), 4096);
    stage(&mut receiver, b"abc");
    push(&mut receiver, 1, &wire(FrameType::ObjectComplete, vec![]));
    wait(|| fixture.root.join("received.bin").exists());
    receiver.cancel(receiver.binding(), 1).unwrap();
    fixture.cx.cancel_fast(CancelKind::User);
    let outcome = event(&mut receiver);
    assert!(matches!(outcome.outcome(), Ok(Completion::Published(_))));
    assert_eq!(outcome.encode_reply(&mut []), Err(Error::BufferTooSmall));
    let mut out = [0; MAX_REPLY_BYTES];
    let length = outcome.encode_reply(&mut out).unwrap().unwrap();
    let proof: ReceiveReceipt = serde_json::from_slice(&parse(&out[..length]).payload).unwrap();
    assert!(proof.committed);
    assert_eq!(fs::read(fixture.root.join("received.bin")).unwrap(), b"abc");
    stop(&mut task);
}

#[test]
fn disconnect_during_partial_transfer_and_silent_expiry_never_publish() {
    for expire in [false, true] {
        let fixture = Fixture::new();
        let (mut receiver, mut task) = fixture.spawn(Policy::conservative(), 4096);
        begin(&mut receiver, 1, &manifest("received.bin", b"abc"));
        push(&mut receiver, 1, &data(0, 0, b"a"));
        event(&mut receiver);
        if expire {
            fixture.clock.advance(3_000_000_000);
            wait(|| task.is_finished());
        } else {
            drop(receiver);
        }
        stop(&mut task);
        fixture.empty();
    }
}

#[test]
fn symlink_destination_racing_verified_manifest_is_never_followed() {
    let fixture = Fixture::new();
    let (mut receiver, mut task) = fixture.spawn(Policy::conservative(), 4096);
    stage(&mut receiver, b"abc");
    fs::write(fixture.root.join("local-file"), b"local data").unwrap();
    symlink("local-file", fixture.root.join("received.bin")).unwrap();
    push(&mut receiver, 1, &wire(FrameType::ObjectComplete, vec![]));
    assert_eq!(
        event(&mut receiver).outcome(),
        Err(worker::Error::Session(session::Error::Storage(
            receive::Error::Conflict
        )))
    );
    stop(&mut task);
    assert_eq!(
        fs::read(fixture.root.join("local-file")).unwrap(),
        b"local data"
    );
    assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 2);
}

#[test]
fn session_quota_is_not_refunded_by_completed_network_transfers() {
    let fixture = Fixture::new();
    let policy = Policy {
        max_session_transfers: 1,
        ..Policy::conservative()
    };
    let (mut receiver, mut task) = fixture.spawn(policy, 4096);
    stage(&mut receiver, b"");
    push(&mut receiver, 1, &wire(FrameType::ObjectComplete, vec![]));
    event(&mut receiver);
    push(&mut receiver, 2, &offered(&manifest("second.bin", b"")));
    assert_eq!(
        event(&mut receiver).outcome(),
        Err(worker::Error::Session(session::Error::Quota))
    );
    stop(&mut task);
    assert!(!fixture.root.join("second.bin").exists());
    assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 1);
}

#[test]
fn invalid_frame_limits_do_not_start_a_worker_or_touch_disk() {
    let fixture = Fixture::new();
    for limit in [0, 1023, 65_537, usize::MAX] {
        assert!(matches!(
            Receiver::spawn(
                fixture.cx.clone(),
                fixture.input.as_ref().unwrap(),
                fixture.directory.clone(),
                Permission::new(true),
                Policy::conservative(),
                limit
            ),
            Err(Error::Limits)
        ));
    }
    fixture.empty();
    fixture.input_live();
}

fn file_settings() -> fr_files::wire::Settings {
    use fr_wire::files::{Context, Direction, Lane, Limits, Role};
    let incoming = Context {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        handle: 3,
        channel: 11,
        sender: Role::Controller,
        direction: Direction::ToHost,
        lane: Lane::Files,
    };
    fr_files::wire::Settings {
        incoming,
        outgoing: Context {
            sender: Role::Host,
            ..incoming
        },
        limits: Limits::new(&fr_core::limits::ProtocolLimits::ABSOLUTE, 4096).unwrap(),
    }
}
fn file_host(fixture: &Fixture) -> (fr_files::wire::HostReceiver, Task) {
    fr_files::wire::HostReceiver::spawn(
        fixture.cx.clone(),
        fixture.input.as_ref().unwrap(),
        fixture.directory.clone(),
        Permission::new(true),
        Policy::conservative(),
        file_settings(),
    )
    .unwrap()
}
fn file_record(id: u64, body: fr_wire::files::Body<'_>) -> Vec<u8> {
    let settings = file_settings();
    let mut bytes = vec![0; settings.limits.record_bytes()];
    let length = fr_wire::files::encode(
        fr_wire::files::Message { id, body },
        settings.incoming,
        settings.limits,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(length);
    bytes
}
fn file_push(host: &mut fr_files::wire::HostReceiver, bytes: &[u8]) {
    // Exercise the real reliable-stream deframer at adversarial read splits,
    // not the assumption that a QUIC/socket read is one application record.
    let settings = file_settings();
    let mut stream = fr_wire::stream::RecordStream::new(
        settings.limits.record_bytes(),
        settings.incoming.channel,
        1_000_000,
    )
    .unwrap();
    for chunk in bytes.chunks(7) {
        assert_eq!(stream.push(chunk, 0).unwrap(), chunk.len());
    }
    let record = stream.frame(0).unwrap().unwrap();
    wait(|| match host.receive(record) {
        Ok(fr_files::wire::Admission::Queued(_)) => true,
        Err(
            fr_files::wire::Error::Busy
            | fr_files::wire::Error::Atp(Error::Worker(worker::Error::Busy)),
        ) => false,
        other => panic!("file admission failed: {other:?}"),
    });
    stream.consume(0).unwrap();
    stream.finish(0).unwrap();
}
fn file_reply(host: &mut fr_files::wire::HostReceiver) -> Vec<u8> {
    let mut bytes = vec![0; 4096];
    let mut length = None;
    wait(|| match host.poll_reply(&mut bytes) {
        Ok(Some(size)) => {
            length = Some(size);
            true
        }
        Ok(None) | Err(fr_files::wire::Error::Atp(Error::Worker(worker::Error::Busy))) => false,
        error => panic!("file reply failed: {error:?}"),
    });
    bytes.truncate(length.unwrap());
    bytes
}
fn file_stage(host: &mut fr_files::wire::HostReceiver, bytes: &[u8]) {
    use fr_wire::files::Body;
    file_push(
        host,
        &file_record(
            1,
            Body::Offer {
                profile: 1,
                atp: &offered(&manifest("received.bin", bytes)),
            },
        ),
    );
    let accepted = file_reply(host);
    let settings = file_settings();
    assert!(matches!(
        fr_wire::files::decode(&accepted, settings.outgoing, settings.limits)
            .unwrap()
            .body,
        Body::Accept { .. }
    ));
    if !bytes.is_empty() {
        file_push(
            host,
            &file_record(
                1,
                Body::Chunk {
                    atp: &data(0, 0, bytes),
                },
            ),
        );
        wait(|| {
            match host.poll_reply(&mut [0; 4096]) {
                Ok(None) => {}
                Err(fr_files::wire::Error::Atp(Error::Worker(worker::Error::Busy))) => {
                    return false;
                }
                other => panic!("unexpected write reply: {other:?}"),
            }
            !host.is_busy()
        });
    }
}

#[test]
fn frd0_envelopes_stream_into_real_files_and_return_bound_atp_proofs() {
    use fr_wire::files::{self, Body, Disposition, Reason};
    let fixture = Fixture::new();
    let (mut host, mut task) = file_host(&fixture);
    let bytes = b"a real file from the original controller";
    file_stage(&mut host, bytes);
    assert_eq!(host.progress().unwrap().staged_bytes, bytes.len() as u64);
    assert!(!fixture.root.join("received.bin").exists());
    file_push(
        &mut host,
        &file_record(
            1,
            Body::Chunk {
                atp: &wire(FrameType::ObjectComplete, vec![]),
            },
        ),
    );
    let response = file_reply(&mut host);
    let settings = file_settings();
    let message = files::decode(&response, settings.outgoing, settings.limits).unwrap();
    assert_eq!(message.id, 1);
    let Body::Complete {
        disposition,
        reason,
        published_bytes,
        atp,
    } = message.body
    else {
        panic!("missing completion")
    };
    assert_eq!(disposition, Disposition::PublishedDurable);
    assert_eq!(reason, Reason::None);
    assert_eq!(published_bytes, bytes.len() as u64);
    let proof: ReceiveReceipt = serde_json::from_slice(&parse(atp).payload).unwrap();
    assert!(proof.committed && proof.sha_ok && proof.merkle_ok);
    assert_eq!(fs::read(fixture.root.join("received.bin")).unwrap(), bytes);
    assert!(matches!(
        host.last_result().unwrap().outcome,
        Ok(Completion::Published(_))
    ));
    stop(&mut task);
}

#[test]
fn short_outer_reply_buffer_retains_result_and_backpressures_new_input() {
    use fr_wire::{
        WireError,
        files::{self, Body},
    };
    let fixture = Fixture::new();
    let (mut host, mut task) = file_host(&fixture);
    file_push(
        &mut host,
        &file_record(
            1,
            Body::Offer {
                profile: 1,
                atp: &offered(&manifest("received.bin", b"abc")),
            },
        ),
    );
    wait(|| match host.poll_reply(&mut []) {
        Err(fr_files::wire::Error::Wire(WireError::BufferTooSmall)) => true,
        Ok(None) | Err(fr_files::wire::Error::Atp(Error::Worker(worker::Error::Busy))) => false,
        other => panic!("unexpected reply: {other:?}"),
    });
    assert!(host.is_busy());
    assert!(matches!(
        host.last_result().unwrap().outcome,
        Ok(Completion::Begun(_))
    ));
    assert_eq!(
        host.receive(&file_record(
            1,
            Body::Chunk {
                atp: &data(0, 0, b"abc")
            }
        )),
        Err(fr_files::wire::Error::Busy)
    );
    let reply = file_reply(&mut host);
    let settings = file_settings();
    let Body::Accept {
        size,
        bytes_per_second,
        chunk_bytes,
        concurrent_transfers,
        ..
    } = files::decode(&reply, settings.outgoing, settings.limits)
        .unwrap()
        .body
    else {
        panic!("accept lost")
    };
    assert_eq!(size, 3);
    assert_eq!(bytes_per_second, Policy::conservative().bytes_per_second);
    assert_eq!(
        chunk_bytes as usize,
        settings.limits.atp_bytes() - files::ATP_DATA_OVERHEAD
    );
    assert_eq!(concurrent_transfers, 1);
    host.stop();
    stop(&mut task);
    fixture.empty();
}

#[test]
fn file_cancel_bypasses_reply_backpressure_without_sending_obsolete_accept() {
    use fr_wire::{
        WireError,
        files::{self, Body, Disposition, Reason},
    };
    let fixture = Fixture::new();
    let (mut host, mut task) = file_host(&fixture);
    file_push(
        &mut host,
        &file_record(
            1,
            Body::Offer {
                profile: 1,
                atp: &offered(&manifest("received.bin", b"abc")),
            },
        ),
    );
    wait(|| match host.poll_reply(&mut []) {
        Err(fr_files::wire::Error::Wire(WireError::BufferTooSmall)) => true,
        Ok(None) | Err(fr_files::wire::Error::Atp(Error::Worker(worker::Error::Busy))) => false,
        other => panic!("unexpected reply: {other:?}"),
    });
    assert_eq!(
        host.receive(&file_record(1, Body::Cancel(Reason::User))),
        Ok(fr_files::wire::Admission::CancellationRequested)
    );
    let reply = file_reply(&mut host);
    let settings = file_settings();
    assert!(matches!(
        files::decode(&reply, settings.outgoing, settings.limits)
            .unwrap()
            .body,
        Body::Complete {
            disposition: Disposition::Refused,
            reason: Reason::Cancelled,
            published_bytes: 0,
            atp: []
        }
    ));
    stop(&mut task);
    fixture.empty();
    fixture.input_live();
}

#[test]
fn completed_file_proof_survives_wire_cancel_and_parent_cancellation() {
    use fr_wire::files::{self, Body, Disposition, Reason};
    let fixture = Fixture::new();
    let (mut host, mut task) = file_host(&fixture);
    file_stage(&mut host, b"abc");
    file_push(
        &mut host,
        &file_record(
            1,
            Body::Chunk {
                atp: &wire(FrameType::ObjectComplete, vec![]),
            },
        ),
    );
    wait(|| fixture.root.join("received.bin").exists());
    assert_eq!(
        host.receive(&file_record(1, Body::Cancel(Reason::User))),
        Ok(fr_files::wire::Admission::CancellationRequested)
    );
    fixture.cx.cancel_fast(CancelKind::User);
    let response = file_reply(&mut host);
    let settings = file_settings();
    assert!(matches!(
        files::decode(&response, settings.outgoing, settings.limits)
            .unwrap()
            .body,
        Body::Complete {
            disposition: Disposition::PublishedDurable,
            published_bytes: 3,
            ..
        }
    ));
    assert!(matches!(
        host.last_result().unwrap().outcome,
        Ok(Completion::Published(_))
    ));
    stop(&mut task);
}

#[test]
fn wrong_outer_channel_handle_session_and_lease_never_reach_atp_or_disk() {
    use fr_wire::files::{Body, Message};
    let fixture = Fixture::new();
    let (mut host, mut task) = file_host(&fixture);
    let settings = file_settings();
    let atp = offered(&manifest("received.bin", b"abc"));
    #[rustfmt::skip]
    let contexts = [
        fr_wire::files::Context { channel: 77, ..settings.incoming },
        fr_wire::files::Context { handle: 77, ..settings.incoming },
        fr_wire::files::Context { session: RemoteSessionId::from_raw(77), ..settings.incoming },
        fr_wire::files::Context { lease: InputLeaseId::from_raw(77), ..settings.incoming },
    ];
    for context in contexts {
        let mut bytes = [0; 4096];
        let size = fr_wire::files::encode(
            Message {
                id: 1,
                body: Body::Offer {
                    profile: 1,
                    atp: &atp,
                },
            },
            context,
            settings.limits,
            &mut bytes,
        )
        .unwrap();
        assert!(matches!(
            host.receive(&bytes[..size]),
            Err(fr_files::wire::Error::Wire(_))
        ));
        assert!(!host.is_busy());
        fixture.empty();
    }
    file_stage(&mut host, b"abc");
    host.stop();
    stop(&mut task);
    fixture.empty();
}

#[test]
fn atp_frames_cannot_change_operation_by_using_a_different_outer_kind() {
    use fr_wire::files::Body;
    for mislabel in [false, true] {
        let fixture = Fixture::new();
        let (mut host, mut task) = file_host(&fixture);
        let record = if mislabel {
            file_stage(&mut host, b"abc");
            file_record(
                1,
                Body::Offer {
                    profile: 1,
                    atp: &wire(FrameType::ObjectComplete, vec![]),
                },
            )
        } else {
            file_record(
                1,
                Body::Chunk {
                    atp: &offered(&manifest("received.bin", b"abc")),
                },
            )
        };
        assert_eq!(
            host.receive(&record),
            Err(fr_files::wire::Error::Atp(Error::Order))
        );
        stop(&mut task);
        fixture.empty();
        fixture.input_live();
    }
}

#[test]
fn atp_integrity_failure_becomes_explicit_frd0_refusal_without_success_proof() {
    use fr_wire::files::{self, Body, Disposition, Reason};
    let fixture = Fixture::new();
    let (mut host, mut task) = file_host(&fixture);
    let mut bad = manifest("received.bin", b"");
    bad.merkle_root_hex = "00".repeat(32);
    file_push(
        &mut host,
        &file_record(
            1,
            Body::Offer {
                profile: 1,
                atp: &offered(&bad),
            },
        ),
    );
    file_reply(&mut host);
    file_push(
        &mut host,
        &file_record(
            1,
            Body::Chunk {
                atp: &wire(FrameType::ObjectComplete, vec![]),
            },
        ),
    );
    let response = file_reply(&mut host);
    let settings = file_settings();
    assert!(matches!(
        files::decode(&response, settings.outgoing, settings.limits)
            .unwrap()
            .body,
        Body::Complete {
            disposition: Disposition::Refused,
            reason: Reason::Integrity,
            published_bytes: 0,
            atp: []
        }
    ));
    stop(&mut task);
    fixture.empty();
}

#[test]
fn observation_or_crossed_attachment_configuration_never_starts_disk_owner() {
    use fr_wire::files::{Context, Direction, Role};
    let fixture = Fixture::new();
    let original = file_settings();
    #[rustfmt::skip]
    let contexts = [
        Context { sender: Role::Observer, ..original.incoming },
        Context { handle: 999, ..original.incoming },
        Context { lease: InputLeaseId::from_raw(999), ..original.incoming },
        Context { direction: Direction::ToController, ..original.incoming },
        Context { channel: 12, ..original.incoming },
    ];
    for incoming in contexts {
        assert!(
            fr_files::wire::HostReceiver::spawn(
                fixture.cx.clone(),
                fixture.input.as_ref().unwrap(),
                fixture.directory.clone(),
                Permission::new(true),
                Policy::conservative(),
                fr_files::wire::Settings {
                    incoming,
                    ..original
                }
            )
            .is_err()
        );
    }
    fixture.empty();
    fixture.input_live();
}
