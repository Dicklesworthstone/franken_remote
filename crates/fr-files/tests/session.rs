#![cfg(target_os = "linux")]
use asupersync::atp::object::ContentId;
use fr_core::{
    authority::{AuthorityError, AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_submission::{Capabilities, InputSession, Refusal},
    time::{HostDuration, HostInstant},
};
use fr_files::{
    receive::{DropDirectory, Limits, Publication},
    session::{Error, HostReceiver, Offer, Permission, Policy},
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
fn at(us: u64) -> HostInstant {
    HostInstant::from_micros(us)
}
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
    let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(at(0)).unwrap();
    a.mark_view_ready(at(0)).unwrap();
    a.grant_lease(c.lease, at(0)).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, at(0)).unwrap();
    InputSession::new(
        a,
        c,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        at(0),
    )
    .unwrap()
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "fr-files-session-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        Self(p)
    }
    fn root(&self) -> DropDirectory {
        DropDirectory::open(
            &self.0,
            Limits {
                max_file_bytes: 1024 * 1024,
                max_reserved_bytes: 1024 * 1024,
                max_transfers: 2,
            },
        )
        .unwrap()
    }
    fn session(&self, input: &InputSession) -> HostReceiver {
        HostReceiver::new(
            input,
            self.root(),
            Permission::new(true),
            Policy::conservative(),
            at(0),
        )
        .unwrap()
    }
    fn empty(&self) {
        assert_eq!(fs::read_dir(&self.0).unwrap().count(), 0);
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn begin(session: &mut HostReceiver, id: u64, bytes: &[u8], now: u64) {
    session
        .begin(
            Offer {
                binding: session.binding(),
                id,
                name: "item",
                size: bytes.len() as u64,
                content: ContentId::from_bytes(bytes),
            },
            || at(now),
        )
        .unwrap();
}
fn staged(session: &mut HostReceiver) {
    begin(session, 1, b"abc", 0);
    session
        .write(session.binding(), 1, 0, b"abc", || at(0))
        .unwrap();
}

#[test]
fn publishes_only_complete_verified_bytes_under_original_control() {
    let scratch = Scratch::new();
    let input = owner();
    let mut session = scratch.session(&input);
    begin(&mut session, 1, b"abc", 0);
    assert_eq!(session.progress().unwrap().staged_bytes, 0);
    assert_eq!(
        session
            .write(session.binding(), 1, 0, b"abc", || at(10))
            .unwrap()
            .staged_bytes,
        3
    );
    assert!(!scratch.0.join("item").exists());
    let receipt = session.complete(session.binding(), 1, || at(11)).unwrap();
    assert_eq!(receipt.bytes, 3);
    assert_eq!(receipt.id, 1);
    assert_eq!(receipt.publication, Publication::Durable);
    assert_eq!(fs::read(scratch.0.join("item")).unwrap(), b"abc");
    assert_eq!(
        session.complete(session.binding(), 1, || at(12)),
        Err(Error::NoTransfer)
    );
    assert!(session.progress().is_none());
    assert!(input.monitor().deadline(at(12)).is_ok());
}

#[test]
fn control_alone_never_enables_file_access() {
    let scratch = Scratch::new();
    let input = owner();
    assert_eq!(
        HostReceiver::new(
            &input,
            scratch.root(),
            Permission::default(),
            Policy::conservative(),
            at(0)
        )
        .unwrap_err(),
        Error::Permission
    );
    scratch.empty();
}

#[test]
fn original_session_and_lease_are_checked_before_writes_and_cancel() {
    let scratch = Scratch::new();
    let input = owner();
    let mut session = scratch.session(&input);
    begin(&mut session, 1, b"abc", 0);
    for changed_session in [false, true] {
        let mut bad = session.binding();
        if changed_session {
            bad.session = RemoteSessionId::from_raw(9);
        } else {
            bad.lease = InputLeaseId::from_raw(9);
        }
        assert_eq!(
            session.write(bad, 1, 0, b"abc", || at(0)),
            Err(Error::WrongBinding)
        );
        assert_eq!(session.cancel(bad, 1), Err(Error::WrongBinding));
        assert_eq!(session.progress().unwrap().staged_bytes, 0);
    }
    session.cancel(session.binding(), 1).unwrap();
    scratch.empty();
}

#[test]
fn permission_revoked_after_disk_verification_prevents_publication() {
    let scratch = Scratch::new();
    let input = owner();
    let permission = Permission::new(true);
    let mut session = HostReceiver::new(
        &input,
        scratch.root(),
        permission.clone(),
        Policy::conservative(),
        at(0),
    )
    .unwrap();
    staged(&mut session);
    let mut calls = 0;
    let result = session.complete(session.binding(), 1, || {
        calls += 1;
        if calls == 2 {
            permission.revoke();
        }
        at(1)
    });
    assert_eq!(calls, 2);
    assert_eq!(result, Err(Error::Permission));
    assert!(session.is_closed());
    scratch.empty();
    assert!(input.monitor().deadline(at(1)).is_ok());
}

#[test]
fn input_revoke_at_final_check_prevents_publication() {
    let scratch = Scratch::new();
    let input = owner();
    let mut session = scratch.session(&input);
    staged(&mut session);
    let mut calls = 0;
    let revoke = input.revoke_handle();
    let result = session.complete(session.binding(), 1, || {
        calls += 1;
        if calls == 2 {
            revoke.revoke();
        }
        at(1)
    });
    assert_eq!(result, Err(Error::Authority(Refusal::Revoked)));
    scratch.empty();
}

#[test]
fn lease_expiring_during_disk_work_is_terminal_without_resurrection() {
    let scratch = Scratch::new();
    let input = owner();
    let mut session = scratch.session(&input);
    staged(&mut session);
    let mut calls = 0;
    let result = session.complete(session.binding(), 1, || {
        calls += 1;
        if calls == 1 { at(1) } else { at(3_000_000) }
    });
    assert_eq!(
        result,
        Err(Error::Authority(Refusal::Authority(
            AuthorityError::LeaseExpired
        )))
    );
    assert!(input.monitor().is_revoked());
    assert!(session.is_closed());
    scratch.empty();
    assert_eq!(
        session.begin(
            Offer {
                binding: session.binding(),
                id: 2,
                name: "item",
                size: 0,
                content: ContentId::from_bytes(b"")
            },
            || at(0)
        ),
        Err(Error::Closed)
    );
}

#[test]
fn transfer_deadline_is_independent_of_live_control() {
    let scratch = Scratch::new();
    let input = owner();
    let policy = Policy {
        transfer_lifetime: HostDuration::from_micros(1000),
        ..Policy::conservative()
    };
    let mut session =
        HostReceiver::new(&input, scratch.root(), Permission::new(true), policy, at(0)).unwrap();
    staged(&mut session);
    let mut calls = 0;
    let result = session.complete(session.binding(), 1, || {
        calls += 1;
        at(if calls == 1 { 999 } else { 1000 })
    });
    assert_eq!(result, Err(Error::Expired));
    assert!(input.monitor().deadline(at(1000)).is_ok());
    scratch.empty();
}

#[test]
fn clock_regression_between_verification_and_rename_refuses() {
    let scratch = Scratch::new();
    let input = owner();
    let mut session = scratch.session(&input);
    staged(&mut session);
    let mut calls = 0;
    let result = session.complete(session.binding(), 1, || {
        calls += 1;
        at(if calls == 1 { 20 } else { 19 })
    });
    assert_eq!(result, Err(Error::Clock));
    scratch.empty();
    assert!(session.is_closed());
}

#[test]
fn controller_drop_and_equal_numeric_replacement_never_revive_transfer() {
    let scratch = Scratch::new();
    let input = owner();
    let mut session = scratch.session(&input);
    staged(&mut session);
    drop(input);
    let replacement = owner();
    assert_eq!(
        session.complete(session.binding(), 1, || at(1)),
        Err(Error::Authority(Refusal::Revoked))
    );
    session.close().unwrap();
    scratch.empty();
    assert!(replacement.monitor().deadline(at(1)).is_ok());
}

#[test]
fn admission_rechecks_permission_after_staging_creation() {
    let scratch = Scratch::new();
    let input = owner();
    let permission = Permission::new(true);
    let mut session = HostReceiver::new(
        &input,
        scratch.root(),
        permission.clone(),
        Policy::conservative(),
        at(0),
    )
    .unwrap();
    let mut calls = 0;
    let result = session.begin(
        Offer {
            binding: session.binding(),
            id: 1,
            name: "item",
            size: 0,
            content: ContentId::from_bytes(b""),
        },
        || {
            calls += 1;
            if calls == 2 {
                permission.revoke();
            }
            at(0)
        },
    );
    assert_eq!(result, Err(Error::Permission));
    assert!(session.progress().is_none());
    scratch.empty();
}

#[test]
fn rate_pressure_is_prewrite_and_same_offset_remains_unconsumed() {
    let scratch = Scratch::new();
    let input = owner();
    let policy = Policy {
        bytes_per_second: 1_000_000,
        burst_bytes: 65_536 + 128,
        ..Policy::conservative()
    };
    let mut session =
        HostReceiver::new(&input, scratch.root(), Permission::new(true), policy, at(0)).unwrap();
    let bytes = vec![0x5a; 65_536];
    begin(&mut session, 1, &bytes, 0);
    session
        .write(session.binding(), 1, 0, &bytes[..32_768], || at(0))
        .unwrap();
    assert_eq!(
        session.write(session.binding(), 1, 32_768, &bytes[32_768..], || at(0)),
        Err(Error::RateLimited)
    );
    assert_eq!(session.progress().unwrap().staged_bytes, 32_768);
    session
        .write(session.binding(), 1, 32_768, &bytes[32_768..], || at(1000))
        .unwrap();
    session.complete(session.binding(), 1, || at(1000)).unwrap();
    assert_eq!(fs::read(scratch.0.join("item")).unwrap(), bytes);
}

#[test]
fn burst_is_capped_and_cancel_is_not_throttled_or_replayed() {
    let scratch = Scratch::new();
    let input = owner();
    let policy = Policy {
        burst_bytes: 65_536 + 128,
        ..Policy::conservative()
    };
    let mut session =
        HostReceiver::new(&input, scratch.root(), Permission::new(true), policy, at(0)).unwrap();
    let bytes = vec![0; 65_537];
    begin(&mut session, 1, &bytes, 0);
    session
        .write(session.binding(), 1, 0, &bytes[..65_536], || at(2_000_000))
        .unwrap();
    assert_eq!(
        session.write(session.binding(), 1, 65_536, &bytes[65_536..], || at(
            2_000_000
        )),
        Err(Error::RateLimited)
    );
    assert_eq!(session.cancel(session.binding(), 0), Err(Error::NoTransfer));
    assert_eq!(session.progress().unwrap().id, 1);
    session.cancel(session.binding(), 1).unwrap();
    scratch.empty();
    assert_eq!(
        session.begin(
            Offer {
                binding: session.binding(),
                id: 1,
                name: "item",
                size: 0,
                content: ContentId::from_bytes(b"")
            },
            || at(2_000_000)
        ),
        Err(Error::Sequence)
    );
    assert!(input.monitor().deadline(at(2_000_000)).is_ok());
}

#[test]
fn local_close_is_terminal_for_files_but_not_desktop_control() {
    let scratch = Scratch::new();
    let input = owner();
    let mut session = scratch.session(&input);
    staged(&mut session);
    session.close().unwrap();
    scratch.empty();
    session.close().unwrap();
    assert!(session.is_closed());
    assert!(input.monitor().deadline(at(1)).is_ok());
    assert_eq!(
        session.begin(
            Offer {
                binding: session.binding(),
                id: 2,
                name: "item",
                size: 0,
                content: ContentId::from_bytes(b"")
            },
            || at(1)
        ),
        Err(Error::Closed)
    );
}

#[test]
fn idle_service_expires_and_reclaims_without_another_peer_record() {
    let scratch = Scratch::new();
    let input = owner();
    let policy = Policy {
        transfer_lifetime: HostDuration::from_micros(1000),
        ..Policy::conservative()
    };
    let mut session =
        HostReceiver::new(&input, scratch.root(), Permission::new(true), policy, at(0)).unwrap();
    begin(&mut session, 1, b"abc", 0);
    session.service(at(999)).unwrap();
    assert!(session.progress().is_some());
    assert_eq!(session.service(at(1000)), Err(Error::Expired));
    assert!(session.progress().is_none());
    scratch.empty();
    assert!(input.monitor().deadline(at(1000)).is_ok());
}

#[test]
fn file_authority_handoff_survives_moving_the_live_owner() {
    let scratch = Scratch::new();
    let input = owner();
    let handoff = fr_files::session::Authority::from_input(&input);
    let (stop, stopped) = std::sync::mpsc::sync_channel::<()>(1);
    let thread = std::thread::spawn(move || {
        let _keep_original_owner = input;
        stopped.recv().unwrap();
    });
    let mut session = HostReceiver::with_authority(
        handoff.clone(),
        scratch.root(),
        Permission::new(true),
        Policy::conservative(),
        at(0),
    )
    .unwrap();
    staged(&mut session);
    assert_eq!(session.binding(), handoff.binding());
    let receipt = session.complete(session.binding(), 1, || at(10)).unwrap();
    assert_eq!(receipt.publication, Publication::Durable);
    assert_eq!(fs::read(scratch.0.join("item")).unwrap(), b"abc");
    stop.send(()).unwrap();
    thread.join().unwrap();
    assert!(handoff.deadline(at(11)).is_err());
}

#[test]
fn cloned_file_authority_cannot_resurrect_a_dropped_equal_id_owner() {
    let scratch = Scratch::new();
    let input = owner();
    let handoff = fr_files::session::Authority::from_input(&input);
    let retained = handoff.clone();
    let mut session = HostReceiver::with_authority(
        handoff,
        scratch.root(),
        Permission::new(true),
        Policy::conservative(),
        at(0),
    )
    .unwrap();
    staged(&mut session);
    drop(input);
    let replacement = owner();
    assert_eq!(
        retained.binding(),
        fr_files::session::Authority::from_input(&replacement).binding()
    );
    assert!(session.complete(session.binding(), 1, || at(10)).is_err());
    assert!(
        HostReceiver::with_authority(
            retained,
            scratch.root(),
            Permission::new(true),
            Policy::conservative(),
            at(10)
        )
        .is_err()
    );
    assert!(!scratch.0.join("item").exists());
    // The original disk worker calls maintenance even after a refused command.
    assert_eq!(session.service(at(10)), Err(Error::Closed));
    scratch.empty();
    assert!(replacement.monitor().deadline(at(10)).is_ok());
}
