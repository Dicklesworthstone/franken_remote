//! Original source authority follows real hashing and the autonomous disk
//! worker. No sender/reactor turn is needed to observe a stopped scope.
use super::tests::{event, fixture, prepared, reap};
use super::*;
use crate::session::{Authority, Permission};
use asupersync::{
    runtime::RuntimeBuilder,
    time::VirtualClock,
    types::{Budget, Time},
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
use std::{fs, path::Path};

struct Owner {
    input: InputSession,
    permission: Permission,
    stopped: Arc<AtomicBool>,
}
impl Owner {
    fn new(now: u64) -> Self {
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
        let now = HostInstant::from_micros(now);
        let mut authority =
            SessionAuthority::new(credentials.session, AuthorityPolicy::plan_defaults());
        authority.mark_capabilities_checked().unwrap();
        authority.authorize_observation(now).unwrap();
        authority.mark_view_ready(now).unwrap();
        authority.grant_lease(credentials.lease, now).unwrap();
        authority
            .issue_input_ticket(credentials.lease, credentials.ticket, now)
            .unwrap();
        Self {
            input: InputSession::new(
                authority,
                credentials,
                InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
                Capabilities::default(),
                now,
            )
            .unwrap(),
            permission: Permission::new(true),
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    fn scope(&self) -> SourceScope {
        SourceScope::new(
            Authority::from_input(&self.input),
            self.permission.clone(),
            self.stopped.clone(),
        )
    }
}

#[test]
fn hashing_stops_at_the_original_permission_boundary_for_files_and_trees() {
    for directory in [false, true] {
        fixture(|path, cx| {
            let pathname = path.join("payload");
            fs::write(&pathname, vec![7_u8; 2 * MAX_CHUNK_BYTES]).unwrap();
            let clock = cx.timer_driver().unwrap();
            let now = clock.now().as_nanos() / 1000;
            let owner = Owner::new(now);
            let gate = Gate {
                stop: Arc::new(AtomicBool::new(false)),
                cx: cx.clone(),
                clock,
                deadline: now + 10_000_000,
                scope: Some(owner.scope()),
                owner_scope: None,
            };
            let root = if directory { path } else { &pathname };
            let mut selected = tree::Selection::new(
                File::open(root).unwrap(),
                "selected",
                directory,
                (2 * MAX_CHUNK_BYTES) as u64,
                &mut || gate.check(),
            )
            .unwrap();
            let tree::Contents::File { file, .. } = &selected.entries[0].contents else {
                panic!("the selected source must contain the real payload");
            };
            // A cloned original descriptor shares its offset with the actual
            // hashing read. Revoke only after one bounded read really happened.
            let mut witness = file.try_clone().unwrap();
            let result = prepare(&mut selected, "selected".into(), directory, &mut || {
                if witness.stream_position().unwrap() != 0 {
                    owner.permission.revoke();
                }
                gate.check()
            });
            assert!(matches!(result, Err(Error::Cancelled)));
            assert_eq!(witness.stream_position().unwrap(), MAX_CHUNK_BYTES as u64);
            assert!(!cx.is_cancel_requested());
            assert!(!owner.input.monitor().is_revoked());
        });
    }
}

#[test]
fn independent_scope_revocation_stops_the_waiting_source_without_a_sender_turn() {
    for directory in [false, true] {
        for reason in 0..3 {
            fixture(|path, cx| {
                let pathname = path.join("payload");
                fs::write(&pathname, b"original source").unwrap();
                let now = cx.timer_driver().unwrap().now().as_nanos() / 1000;
                let owner = Owner::new(now);
                let root = if directory { path } else { &pathname };
                let mut source = Source::spawn_selected(
                    cx.clone(),
                    SelectedSource {
                        file: File::open(root).unwrap(),
                        name: "selected".into(),
                        directory,
                        expected_bytes: Some(15),
                    },
                    Policy::default(),
                    now + 10_000_000,
                    Some(owner.scope()),
                    None,
                )
                .unwrap();
                assert_eq!(prepared(&mut source).total_bytes, 15);
                match reason {
                    0 => owner.permission.revoke(),
                    1 => owner.stopped.store(true, Ordering::Release),
                    _ => owner.input.revoke_handle().revoke(),
                }
                // The worker checks its original scope while waiting for a
                // command. No request, cancel, or reactor service follows.
                reap(&mut source);
                assert!(matches!(source.poll(), Err(Error::Cancelled)));
                assert!(!cx.is_cancel_requested());
                if reason != 2 {
                    assert!(!owner.input.monitor().is_revoked());
                }
            });
        }
    }
}

#[test]
fn source_lease_expiry_is_independent_of_the_later_transfer_deadline() {
    fixture(|path, _cx| {
        let pathname = path.join("payload");
        fs::write(&pathname, b"original source").unwrap();
        let clock = Arc::new(VirtualClock::new());
        let timer = TimerDriverHandle::with_virtual_clock(clock.clone());
        let runtime = RuntimeBuilder::new()
            .worker_threads(1)
            .with_timer_driver(timer)
            .build()
            .unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let owner = Owner::new(0);
        let mut source = Source::spawn_selected(
            cx.clone(),
            SelectedSource {
                file: File::open(pathname).unwrap(),
                name: "selected".into(),
                directory: false,
                expected_bytes: Some(15),
            },
            Policy::default(),
            10_000_000,
            Some(owner.scope()),
            None,
        )
        .unwrap();
        assert_eq!(prepared(&mut source).total_bytes, 15);
        // The plan-default lease expires at three seconds. There is no owner
        // service to renew it; the source's transfer deadline remains seven
        // seconds away and the source Cx itself remains alive.
        clock.advance_to(Time::from_millis(3_000));
        reap(&mut source);
        assert!(matches!(source.poll(), Err(Error::Cancelled)));
        assert!(!cx.is_cancel_requested());
        assert!(owner.permission.is_approved());
        assert!(!owner.stopped.load(Ordering::Acquire));
    });
}

#[test]
fn original_transfer_deadline_still_bounds_a_valid_source_scope() {
    fixture(|_path, cx| {
        let clock = Arc::new(VirtualClock::new());
        let owner = Owner::new(0);
        let gate = Gate {
            stop: Arc::new(AtomicBool::new(false)),
            cx: cx.clone(),
            clock: TimerDriverHandle::with_virtual_clock(clock.clone()),
            deadline: 1_000,
            scope: Some(owner.scope()),
            owner_scope: None,
        };
        assert_eq!(gate.check(), Ok(()));
        clock.advance_to(Time::from_millis(1));
        assert_eq!(gate.check(), Err(Error::Expired));
        assert_eq!(owner.scope().check(1_000), Ok(()));
        assert!(!cx.is_cancel_requested());
    });
}

fn real_source_event(path: &Path, cx: &Cx, chunk: bool) -> Event {
    let pathname = path.join("payload");
    fs::write(&pathname, b"original source").unwrap();
    let now = cx.timer_driver().unwrap().now().as_nanos() / 1000;
    let mut original = Source::spawn(
        cx.clone(),
        File::open(pathname).unwrap(),
        "selected".into(),
        Policy::default(),
        now + 10_000_000,
    )
    .unwrap();
    let prepared = event(&mut original).unwrap();
    assert!(matches!(&prepared, Event::Prepared(_)));
    let result = if chunk {
        original.request(MAX_CHUNK_BYTES).unwrap();
        let chunk = event(&mut original).unwrap();
        assert!(matches!(&chunk, Event::Chunk { .. }));
        chunk
    } else {
        prepared
    };
    original.stop();
    reap(&mut original);
    result
}

#[test]
fn unread_prepared_and_chunk_results_cannot_mask_source_revocation_or_expiry() {
    for chunk in [false, true] {
        for expired in [false, true] {
            fixture(|path, cx| {
                let unread = real_source_event(path, cx, chunk);
                // This is a mailbox custody fixture, not a transport proof.
                // Keep a genuine disk-worker result unread in the same bounded
                // mailbox, then deterministically stop the original scope. The
                // controlled worker uses production failure recording; no
                // timing assumption or synthetic publication is involved.
                let clock = Arc::new(VirtualClock::new());
                let owner = Owner::new(0);
                let stop = Arc::new(AtomicBool::new(false));
                let gate = Gate {
                    stop: stop.clone(),
                    cx: cx.clone(),
                    clock: TimerDriverHandle::with_virtual_clock(clock.clone()),
                    deadline: 1_000,
                    scope: Some(owner.scope()),
                    owner_scope: None,
                };
                let (command, _requests) = mpsc::sync_channel(1);
                let (reply, result) = mpsc::sync_channel(1);
                let (ready, queued) = mpsc::sync_channel(1);
                let (release, resume) = mpsc::sync_channel(1);
                let (failed, failure_recorded) = mpsc::sync_channel(1);
                let (finish, cleanup) = mpsc::sync_channel(1);
                let failure = Arc::new(Mutex::new(None));
                let worker_failure = failure.clone();
                let thread = thread::spawn(move || {
                    assert!(reply.try_send(Ok(unread)).is_ok());
                    ready.send(()).unwrap();
                    resume.recv().unwrap();
                    let error = gate.check().unwrap_err();
                    record_failure(error, &worker_failure, &reply);
                    failed.send(()).unwrap();
                    // Operation failure is observable before cleanup finishes.
                    let _ = cleanup.recv();
                });
                let mut source = Source {
                    command,
                    result,
                    stop,
                    failure,
                    thread: Some(thread),
                    pending: true,
                };
                queued.recv_timeout(Duration::from_secs(2)).unwrap();
                let expected = if expired {
                    clock.advance_to(Time::from_millis(1));
                    Error::Expired
                } else {
                    owner.permission.revoke();
                    Error::Cancelled
                };
                release.send(()).unwrap();
                failure_recorded
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
                assert_eq!(source.request(1), Err(expected));
                assert!(matches!(source.poll(), Err(error) if error == expected));
                assert!(!source.pending());
                assert!(matches!(source.result.try_recv(), Err(TryRecvError::Empty)));
                assert!(!source.finished());
                assert_eq!(source.reap(), None);

                finish.send(()).unwrap();
                reap(&mut source);
                assert!(source.finished());
                assert_eq!(source.reap(), None);
                assert!(matches!(source.poll(), Err(error) if error == expected));
                assert_eq!(source.request(1), Err(expected));
                assert!(!cx.is_cancel_requested());
                assert!(!owner.input.monitor().is_revoked());
            });
        }
    }
}
