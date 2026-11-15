//! Actual native disk ownership under the existing ATP receiver and original
//! input authority. The monotonic clock is deterministic; filesystem effects
//! and worker cleanup are real. No network or Windows input injection runs.
use super::{Scratch, limits};
use asupersync::{
    bytes::BytesMut,
    codec::Decoder,
    cx::Cx,
    net::atp::{
        protocol::{
            codec::AtpFrameCodec,
            frames::{Frame, FrameType, ProtocolVersion},
        },
        transport_common::{
            EntryMetadata, FileKind, StagedEntryReceive, flat_merkle_root_from_digests, hex_encode,
            metadata_commitment,
        },
        transport_tcp::{ManifestEntry, ReceiveReceipt, TransferManifest},
    },
    runtime::{Runtime, RuntimeBuilder},
    time::{TimerDriverHandle, VirtualClock},
    types::Budget,
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, InputTicketId,
        RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{DesktopPoint, InputBounds, InputCredentials, InputView},
    input_submission::{Capabilities, InputSession},
    time::HostInstant,
};
use fr_files::{
    atp::receive::{Error, Event, MAX_FRAME_BYTES, MAX_REPLY_BYTES, Receiver},
    receive::{DirectoryManifest, DropDirectory, Publication},
    session::{Permission, Policy},
    worker::{self, Completion, Task},
};
use std::{
    fs,
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

fn owner() -> InputSession {
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
    let now = HostInstant::from_micros(0);
    let mut authority =
        SessionAuthority::new(credentials.session, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(now).unwrap();
    authority.mark_view_ready(now).unwrap();
    authority.grant_lease(credentials.lease, now).unwrap();
    authority
        .issue_input_ticket(credentials.lease, credentials.ticket, now)
        .unwrap();
    InputSession::new(
        authority,
        credentials,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        now,
    )
    .unwrap()
}

struct Fixture {
    // Rust drops these fields in declaration order: close all retained roots
    // before Scratch attempts to remove its original test directory.
    directory: DropDirectory,
    input: Option<InputSession>,
    permission: Permission,
    clock: Arc<VirtualClock>,
    cx: Cx,
    _runtime: Runtime,
    scratch: Scratch,
}
impl Fixture {
    fn new() -> Self {
        let scratch = Scratch::new();
        let directory = DropDirectory::open(&scratch.0, limits()).unwrap();
        let clock = Arc::new(VirtualClock::new());
        let runtime = RuntimeBuilder::new()
            .worker_threads(1)
            .with_timer_driver(TimerDriverHandle::with_virtual_clock(clock.clone()))
            .build()
            .unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        Self {
            directory,
            input: Some(owner()),
            permission: Permission::new(true),
            clock,
            cx,
            _runtime: runtime,
            scratch,
        }
    }

    fn spawn(&self) -> (Receiver, Task) {
        Receiver::spawn(
            self.cx.clone(),
            self.input.as_ref().unwrap(),
            self.directory.clone(),
            self.permission.clone(),
            Policy::conservative(),
            8192,
        )
        .unwrap()
    }
}

fn wait(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "bounded worker cleanup expired");
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
    let mut input = BytesMut::from(bytes);
    let frame = AtpFrameCodec::with_max_frame_size(MAX_FRAME_BYTES as u64)
        .decode(&mut input)
        .unwrap()
        .unwrap();
    assert!(input.is_empty());
    frame
}

fn manifest(name: &str, bytes: &[u8]) -> TransferManifest {
    let mut digest = StagedEntryReceive::new(PathBuf::from("test-hash-only"));
    digest.update_with_chunk(bytes);
    let (digest, _, _) = digest.finalize(name.into());
    TransferManifest {
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

fn offer(manifest: &TransferManifest) -> Vec<u8> {
    wire(
        FrameType::ObjectManifest,
        serde_json::to_vec(manifest).unwrap(),
    )
}

fn data(offset: u64, bytes: &[u8]) -> Vec<u8> {
    let mut payload = 0_u32.to_be_bytes().to_vec();
    payload.extend_from_slice(&offset.to_be_bytes());
    payload.extend_from_slice(bytes);
    wire(FrameType::ObjectData, payload)
}

fn push(receiver: &mut Receiver, frame: &[u8]) {
    wait(|| match receiver.push(receiver.binding(), 1, frame) {
        Ok(_) => true,
        Err(Error::Busy | Error::Worker(worker::Error::Busy)) => false,
        Err(error) => panic!("frame refused: {error:?}"),
    });
}

fn event(receiver: &mut Receiver) -> Event {
    let mut result = None;
    wait(|| match receiver.poll() {
        Ok(Some(event)) => {
            result = Some(event);
            true
        }
        Ok(None) | Err(Error::Worker(worker::Error::Busy)) => false,
        Err(error) => panic!("worker poll failed: {error:?}"),
    });
    result.unwrap()
}

fn stop(task: &mut Task) {
    task.stop();
    wait(|| task.is_finished());
    assert_eq!(task.try_finish_cleanup(), Some(Ok(())));
}

#[test]
fn real_atp_worker_commits_complete_file_and_reports_uncertain_directory_durability() {
    let fixture = Fixture::new();
    let (mut receiver, mut task) = fixture.spawn();
    let bytes: Vec<_> = (0..65_539).map(|i| u8::try_from(i % 251).unwrap()).collect();
    let manifest = manifest("received.bin", &bytes);
    push(&mut receiver, &offer(&manifest));
    let accepted = event(&mut receiver);
    assert!(matches!(accepted.outcome(), Ok(Completion::Begun(_))));
    let mut reply = [0; MAX_REPLY_BYTES];
    let length = accepted.encode_reply(&mut reply).unwrap().unwrap();
    assert_eq!(parse(&reply[..length]).frame_type(), FrameType::ObjectRequest);

    let mut offset = 0;
    for chunk in bytes.chunks(7000) {
        fixture.clock.advance(20_000_000);
        push(&mut receiver, &data(offset, chunk));
        let outcome = event(&mut receiver);
        let Ok(Completion::Written(progress)) = outcome.outcome() else {
            panic!("native write did not complete");
        };
        offset += chunk.len() as u64;
        assert_eq!(progress.staged_bytes, offset);
        assert!(!fixture.scratch.0.join("received.bin").exists());
    }
    push(&mut receiver, &wire(FrameType::ObjectComplete, vec![]));
    let result = event(&mut receiver);
    let Ok(Completion::Published(receipt)) = result.outcome() else {
        panic!("native publication did not complete");
    };
    assert_eq!(receipt.publication, Publication::DurabilityUnknown);
    assert_eq!(fs::read(fixture.scratch.0.join("received.bin")).unwrap(), bytes);
    let length = result.encode_reply(&mut reply).unwrap().unwrap();
    let frame = parse(&reply[..length]);
    assert_eq!(frame.frame_type(), FrameType::Proof);
    let proof: ReceiveReceipt = serde_json::from_slice(&frame.payload).unwrap();
    assert!(proof.committed && proof.sha_ok && proof.merkle_ok);
    assert_eq!(proof.bytes_received, bytes.len() as u64);
    assert_eq!(proof.files, 1);
    stop(&mut task);
}

#[test]
fn valid_directory_profiles_refuse_before_worker_admission_or_disk_reservation() {
    for profile in [
        fr_wire::files::ATP_PORTABLE_DIRECTORY_FULL,
        fr_wire::files::ATP_PORTABLE_DIRECTORY_ENTRIES,
    ] {
        let fixture = Fixture::new();
        let (mut receiver, mut task) = fixture.spawn();
        let mut manifest = manifest("leaf", b"");
        manifest.root_name = "tree".into();
        manifest.is_directory = true;
        if profile == fr_wire::files::ATP_PORTABLE_DIRECTORY_ENTRIES {
            let metadata = EntryMetadata {
                file_kind: FileKind::Directory,
                ..EntryMetadata::default()
            };
            let commitment = metadata_commitment(&[("leaf", &metadata)]).unwrap();
            manifest.entries[0].metadata = Some(metadata);
            manifest.metadata_root_hex = Some(commitment.clone());
            assert!(DirectoryManifest::with_metadata(
                manifest.root_name.clone(),
                0,
                manifest.entries.clone(),
                manifest.merkle_root_hex.clone(),
                commitment,
            )
            .is_ok());
        } else {
            assert!(DirectoryManifest::new(
                manifest.root_name.clone(),
                0,
                manifest.entries.clone(),
                manifest.merkle_root_hex.clone(),
            )
            .is_ok());
        }
        assert_eq!(
            receiver.push_profile(receiver.binding(), 1, &offer(&manifest), profile),
            Err(Error::UnsupportedProfile)
        );
        assert!(!receiver.has_pending());
        assert_eq!(fixture.scratch.entries(), 0);
        stop(&mut task);
        super::verified(&fixture.directory, "still-available", b"")
            .cancel()
            .unwrap();
    }
}

#[test]
fn original_authority_loss_expiry_and_permission_revoke_clean_native_staging() {
    for reason in 0..3 {
        let mut fixture = Fixture::new();
        let (mut receiver, mut task) = fixture.spawn();
        push(&mut receiver, &offer(&manifest("unfinished", b"abc")));
        assert!(matches!(event(&mut receiver).outcome(), Ok(Completion::Begun(_))));
        push(&mut receiver, &data(0, b"a"));
        assert!(matches!(event(&mut receiver).outcome(), Ok(Completion::Written(_))));
        assert!(!fixture.scratch.0.join("unfinished").exists());
        match reason {
            0 => drop(fixture.input.take()),
            1 => {
                fixture.clock.advance(4_000_000_000);
            }
            _ => fixture.permission.revoke(),
        }
        wait(|| task.is_finished());
        assert_eq!(task.try_finish_cleanup(), Some(Ok(())));
        assert!(receiver.is_closed());
        assert_eq!(fixture.scratch.entries(), 0);
        super::verified(&fixture.directory, "new-storage-reservation", b"")
            .cancel()
            .unwrap();
    }
}
