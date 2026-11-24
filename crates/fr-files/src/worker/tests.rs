use super::*;
use crate::receive::{DirectoryManifest, Limits};
use asupersync::{
    net::atp::{
        transport_common::{StagedEntryReceive, flat_merkle_root_from_digests, hex_encode},
        transport_tcp::ManifestEntry,
    },
    runtime::{Runtime, RuntimeBuilder},
    time::VirtualClock,
    types::{Budget, CancelKind, Time},
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_submission::Capabilities,
};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    path: PathBuf,
    root: DropDirectory,
    shared: Shared,
    clock: Arc<VirtualClock>,
    input: InputSession,
    _runtime: Runtime,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fr-files-clock-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let root = DropDirectory::open(
            &path,
            Limits {
                max_file_bytes: 3,
                max_reserved_bytes: 3,
                max_transfers: 1,
            },
        )
        .unwrap();
        let clock = Arc::new(VirtualClock::new());
        let timer = TimerDriverHandle::with_virtual_clock(clock.clone());
        let runtime = RuntimeBuilder::new()
            .worker_threads(1)
            .with_timer_driver(timer.clone())
            .build()
            .unwrap();
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
        let input = InputSession::new(
            authority,
            credentials,
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            Capabilities::default(),
            now,
        )
        .unwrap();
        let shared = Shared {
            inbox: Mutex::new(Inbox {
                next: 1,
                slot: Slot::Idle,
            }),
            permission: Permission::new(true),
            cx: runtime.request_cx_with_budget(Budget::INFINITE),
            clock: timer,
            authority: session::Authority::from_input(&input),
            last_sample: AtomicU64::new(0),
            round: RoundDeadline::default(),
            wake: OnceLock::new(),
        };
        Self {
            path,
            root,
            shared,
            clock,
            input,
            _runtime: runtime,
        }
    }

    fn staged(&self, directory: bool) -> HostReceiver {
        let mut receiver = HostReceiver::with_authority(
            self.shared.authority.clone(),
            self.root.clone(),
            self.shared.permission.clone(),
            Policy::conservative(),
            HostInstant::from_micros(0),
        )
        .unwrap();
        let expected = if directory {
            let mut hash = StagedEntryReceive::new("hash-only".into());
            hash.update_with_chunk(b"abc");
            let (digest, _, _) = hash.finalize("nested/file".into());
            let entry = ManifestEntry {
                index: 0,
                rel_path: "nested/file".into(),
                size: 3,
                sha256_hex: hex_encode(&digest.content_sha256),
                metadata: None,
                members: vec![],
            };
            Expected::Directory(
                DirectoryManifest::new(
                    "item".into(),
                    3,
                    vec![entry],
                    flat_merkle_root_from_digests(&[digest]),
                )
                .unwrap(),
            )
        } else {
            Expected::Content(ContentId::from_bytes(b"abc"))
        };
        receiver
            .begin_verified(
                VerifiedOffer {
                    binding: receiver.binding(),
                    id: 1,
                    name: "item",
                    size: 3,
                    expected,
                },
                || self.shared.sample(),
            )
            .unwrap();
        receiver
            .write_entry(receiver.binding(), 1, 0, 0, b"abc", || self.shared.sample())
            .unwrap();
        receiver
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).unwrap();
    }
}

#[test]
fn cancellation_during_either_projection_fences_before_returning_a_clock() {
    for publication in [false, true] {
        let fixture = Fixture::new();
        fixture.clock.advance_to(Time::from_millis(1));
        let shared = &fixture.shared;
        let now = shared.sample_with(publication, |local| {
            let sample = shared.authority.sample(local);
            shared.cx.cancel_fast(CancelKind::User);
            sample
        });
        assert_eq!(now, HostInstant::from_micros(0));
        assert_eq!(shared.last_sample.load(Ordering::Acquire), 0);
        assert!(!shared.permission.is_approved());
        assert!(fixture.input.monitor().deadline(now).is_ok());
    }
}

#[test]
fn cancellation_during_contention_stops_publication_projection_retries() {
    let fixture = Fixture::new();
    let shared = &fixture.shared;
    let mut attempts = 0;
    let now = shared.sample_with(true, |_| {
        attempts += 1;
        shared.cx.cancel_fast(CancelKind::User);
        Err(Refusal::AuthorityUnavailable)
    });
    assert_eq!(attempts, 1);
    assert_eq!(now, HostInstant::from_micros(0));
    assert!(!shared.permission.is_approved());
}

#[test]
fn live_contention_retries_take_fresh_clocks_and_keep_the_original_bound() {
    let fixture = Fixture::new();
    let shared = &fixture.shared;
    let mut samples = Vec::new();
    let now = shared.sample_with(true, |local| {
        samples.push(local);
        if samples.len() < 3 {
            fixture
                .clock
                .advance_to(Time::from_millis(samples.len() as u64));
            Err(Refusal::AuthorityUnavailable)
        } else {
            shared.authority.try_sample(local)
        }
    });
    assert_eq!(samples, vec![0, 1000, 2000]);
    assert_eq!(now, HostInstant::from_micros(2000));
    assert_eq!(shared.last_sample.load(Ordering::Acquire), 2000);
    assert!(shared.permission.is_approved());

    let mut attempts = 0;
    let refused = shared.sample_with(true, |_| {
        attempts += 1;
        Err(Refusal::AuthorityUnavailable)
    });
    assert_eq!(attempts, CONTENDED_RETRIES + 1);
    assert_eq!(refused, now);
    assert!(!shared.permission.is_approved());
}

#[test]
fn cancellation_inside_each_completion_projection_blocks_file_and_tree_publication() {
    for directory in [false, true] {
        // Admission, publication policy, and finally the last native rename
        // clock. The last case has no later callback to notice cancellation.
        for cancel_at in 1..=3 {
            let fixture = Fixture::new();
            let mut receiver = fixture.staged(directory);
            let shared = &fixture.shared;
            let mut clocks = 0;
            let result = receiver.complete(receiver.binding(), 1, || {
                shared.sample_with(true, |local| {
                    clocks += 1;
                    let sample = shared.authority.try_sample(local);
                    if clocks == cancel_at {
                        shared.cx.cancel_fast(CancelKind::User);
                    }
                    sample
                })
            });
            assert_eq!(result, Err(session::Error::Permission));
            assert_eq!(clocks, cancel_at);
            assert!(!fixture.path.join("item").exists());
            receiver.close().unwrap();
            assert_eq!(fs::read_dir(&fixture.path).unwrap().count(), 0);
            // Both the three-byte quota and the only transfer slot return.
            fixture
                .root
                .begin("next", 3, ContentId::from_bytes(b"abc"))
                .unwrap()
                .cancel()
                .unwrap();
            assert!(
                fixture
                    .input
                    .monitor()
                    .deadline(HostInstant::from_micros(0))
                    .is_ok()
            );
        }
    }
}

#[test]
fn round_expiry_inside_the_last_publication_projection_preserves_the_destination() {
    let fixture = Fixture::new();
    let mut receiver = fixture.staged(false);
    let shared = &fixture.shared;
    assert!(shared.round.start(1_000));
    let mut clocks = 0;
    let result = receiver.complete(receiver.binding(), 1, || {
        shared.sample_with(true, |local| {
            clocks += 1;
            let sample = shared.authority.try_sample(local);
            if clocks == 3 {
                fixture.clock.advance_to(Time::from_millis(1));
            }
            sample
        })
    });
    assert_eq!(result, Err(session::Error::Permission));
    assert_eq!(clocks, 3);
    assert!(!shared.round.finish(999));
    assert!(!shared.round.start(2_000));
    assert!(!fixture.path.join("item").exists());
    receiver.close().unwrap();
    assert_eq!(fs::read_dir(&fixture.path).unwrap().count(), 0);
    assert!(
        fixture
            .input
            .monitor()
            .deadline(HostInstant::from_micros(1000))
            .is_ok()
    );
}

#[test]
fn only_an_unexpired_round_can_release_its_deadline_for_the_next_round() {
    let round = RoundDeadline::default();
    assert!(round.start(100));
    assert!(!round.start(200));
    assert!(round.finish(99));
    assert!(round.start(200));
    assert!(!round.elapsed(100));
    assert!(round.elapsed(200));
    assert!(!round.finish(199));
    assert!(!round.start(300));
}
