#![cfg(target_os = "linux")]
use asupersync::{
    atp::object::ContentId,
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    time::{TimerDriverHandle, VirtualClock},
    types::{Budget, CancelKind, Time},
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_submission::{Capabilities, InputSession},
    time::HostInstant,
};
use fr_files::{
    receive::{DropDirectory, Limits, MAX_CHUNK_BYTES, Publication},
    session::{Permission, Policy},
    worker::{self, Completion, Error, Mailbox, Task},
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    path: PathBuf,
    cx: Cx,
    clock: Arc<VirtualClock>,
    input: InputSession,
    _runtime: Runtime,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fr-files-worker-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let clock = Arc::new(VirtualClock::new());
        let runtime = RuntimeBuilder::new()
            .worker_threads(1)
            .with_timer_driver(TimerDriverHandle::with_virtual_clock(clock.clone()))
            .build()
            .unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
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
        let input = InputSession::new(
            a,
            c,
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            Capabilities::default(),
            now,
        )
        .unwrap();
        Self {
            path,
            cx,
            clock,
            input,
            _runtime: runtime,
        }
    }
    fn spawn(&self) -> (Mailbox, Task) {
        let root = DropDirectory::open(
            &self.path,
            Limits {
                max_file_bytes: 1024 * 1024,
                max_reserved_bytes: 1024 * 1024,
                max_transfers: 1,
            },
        )
        .unwrap();
        worker::spawn(
            self.cx.clone(),
            &self.input,
            root,
            Permission::new(true),
            Policy::conservative(),
        )
        .unwrap()
    }
    fn empty(&self) {
        assert_eq!(fs::read_dir(&self.path).unwrap().count(), 0);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).unwrap();
    }
}
fn enqueue(mut submit: impl FnMut() -> Result<u64, Error>) -> u64 {
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        match submit() {
            Ok(id) => return id,
            Err(Error::Busy) if Instant::now() < end => std::thread::yield_now(),
            e => panic!("handoff: {e:?}"),
        }
    }
}
fn collect(mailbox: &Mailbox, sequence: u64) -> Completion {
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        match mailbox.take_receipt() {
            Ok(Some(receipt)) => {
                assert_eq!(receipt.sequence, sequence);
                return receipt.result.unwrap();
            }
            Ok(None) | Err(Error::Busy) if Instant::now() < end => std::thread::yield_now(),
            e => panic!("receipt: {e:?}"),
        }
    }
}
fn finish(task: &mut Task) {
    let end = Instant::now() + Duration::from_secs(2);
    while task.try_finish().is_none() {
        assert!(Instant::now() < end, "disk worker did not finish");
        std::thread::yield_now();
    }
}
fn stage(mailbox: &Mailbox) {
    let seq = enqueue(|| mailbox.begin(1, "file", 3, ContentId::from_bytes(b"abc")));
    assert!(matches!(collect(mailbox, seq), Completion::Begun(_)));
    let seq = enqueue(|| mailbox.write_chunk(1, 0, b"abc"));
    assert!(matches!(collect(mailbox,seq),Completion::Written(p) if p.staged_bytes==3));
}
#[test]
fn disk_receipt_publishes_verified_bytes_and_preserves_input_authority() {
    let f = Fixture::new();
    let (m, mut t) = f.spawn();
    stage(&m);
    assert!(!f.path.join("file").exists());
    let seq = enqueue(|| m.complete(1));
    assert!(
        matches!(collect(&m,seq),Completion::Published(r) if r.bytes==3 && r.id==1 && r.publication==Publication::Durable)
    );
    assert_eq!(fs::read(f.path.join("file")).unwrap(), b"abc");
    m.stop();
    finish(&mut t);
    assert!(
        f.input
            .monitor()
            .deadline(HostInstant::from_micros(0))
            .is_ok()
    );
}
#[test]
fn single_slot_includes_queued_executing_and_uncollected_result() {
    let f = Fixture::new();
    let (m, mut t) = f.spawn();
    let seq = enqueue(|| m.begin(1, "file", 0, ContentId::from_bytes(b"")));
    for _ in 0..100 {
        assert_eq!(
            m.begin(2, "second", 0, ContentId::from_bytes(b"")),
            Err(Error::Busy)
        );
        std::thread::yield_now();
    }
    assert!(matches!(collect(&m, seq), Completion::Begun(_)));
    let seq = enqueue(|| m.cancel(1));
    assert_eq!(collect(&m, seq), Completion::Cancelled);
    m.stop();
    finish(&mut t);
    f.empty();
}
#[test]
fn checks_name_and_chunk_bounds_before_copy_or_disk_work() {
    let f = Fixture::new();
    let (m, mut t) = f.spawn();
    for name in ["../escape", "/absolute", "NUL.txt", ".fr-part-mine"] {
        assert_eq!(
            m.begin(1, name, 0, ContentId::from_bytes(b"")),
            Err(Error::InvalidName)
        );
    }
    assert_eq!(
        m.write_chunk(1, 0, &vec![0; MAX_CHUNK_BYTES + 1]),
        Err(Error::InvalidChunk)
    );
    assert_eq!(m.write_chunk(1, 0, b""), Err(Error::InvalidChunk));
    m.stop();
    finish(&mut t);
    f.empty();
}
#[test]
fn no_traffic_lease_expiry_cleans_staging_and_finishes_worker() {
    let f = Fixture::new();
    let (m, mut t) = f.spawn();
    stage(&m);
    f.clock.advance_to(Time::from_millis(3000));
    finish(&mut t);
    f.empty();
    assert_eq!(m.complete(1), Err(Error::Closed));
}
#[test]
fn parent_cancellation_cleans_staging_without_revoking_input() {
    let f = Fixture::new();
    let (m, mut t) = f.spawn();
    stage(&m);
    f.cx.cancel_fast(CancelKind::User);
    finish(&mut t);
    f.empty();
    assert_eq!(m.complete(1), Err(Error::Closed));
    assert!(
        f.input
            .monitor()
            .deadline(HostInstant::from_micros(0))
            .is_ok()
    );
}
#[test]
fn mailbox_drop_requests_cleanup_and_task_can_be_supervised() {
    let f = Fixture::new();
    let (m, mut t) = f.spawn();
    stage(&m);
    drop(m);
    finish(&mut t);
    f.empty();
}
#[test]
fn completed_publication_receipt_survives_stop_and_thread_exit() {
    let f = Fixture::new();
    let (m, mut t) = f.spawn();
    stage(&m);
    let seq = enqueue(|| m.complete(1));
    let end = Instant::now() + Duration::from_secs(2);
    while !f.path.join("file").exists() {
        assert!(Instant::now() < end);
        std::thread::yield_now();
    }
    m.stop();
    finish(&mut t);
    assert!(matches!(collect(&m,seq),Completion::Published(r) if r.bytes==3));
    assert_eq!(fs::read(f.path.join("file")).unwrap(), b"abc");
}
#[test]
fn original_owner_revoke_blocks_publication_without_waiting_for_peer() {
    let f = Fixture::new();
    let (m, mut t) = f.spawn();
    stage(&m);
    f.input.revoke_handle().revoke();
    finish(&mut t);
    f.empty();
    assert_eq!(m.complete(1), Err(Error::Closed));
}

#[test]
fn upstream_atp_frames_flow_through_worker_to_real_verified_publication() {
    let f = Fixture::new();
    let (m, mut t) = f.spawn();
    let bytes = vec![42; 9_000];
    let seq = enqueue(|| {
        m.begin(
            1,
            "atp-file",
            bytes.len() as u64,
            ContentId::from_bytes(&bytes),
        )
    });
    assert!(matches!(collect(&m, seq), Completion::Begun(_)));
    let mut offset = 0;
    for data in bytes.chunks(2_048) {
        let record = fr_files::atp::encode_data(offset, data).unwrap();
        let seq = enqueue(|| m.atp_record(1, &record));
        offset += data.len() as u64;
        assert!(matches!(collect(&m,seq),Completion::Written(p) if p.staged_bytes==offset));
        assert!(!f.path.join("atp-file").exists());
    }
    let record = fr_files::atp::encode_complete().unwrap();
    let seq = enqueue(|| m.atp_record(1, &record));
    assert!(matches!(collect(&m,seq),Completion::Published(r) if r.bytes==bytes.len() as u64));
    assert_eq!(fs::read(f.path.join("atp-file")).unwrap(), bytes);
    m.stop();
    finish(&mut t);
}
#[test]
fn malformed_atp_is_terminal_and_cleans_the_private_partial_file() {
    let f = Fixture::new();
    let (m, mut t) = f.spawn();
    stage(&m);
    let seq = enqueue(|| m.atp_record(1, b"not ATP"));
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        match m.take_receipt() {
            Ok(Some(r)) => {
                assert_eq!(r.sequence, seq);
                assert_eq!(r.result, Err(fr_files::session::Error::Protocol));
                break;
            }
            Ok(None) | Err(Error::Busy) if Instant::now() < until => std::thread::yield_now(),
            e => panic!("receipt: {e:?}"),
        }
    }
    finish(&mut t);
    f.empty();
    assert_eq!(m.complete(1), Err(Error::Closed));
}
