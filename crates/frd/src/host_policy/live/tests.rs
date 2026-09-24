//! Actual Store I/O and owned disk threads; delayed/panicking readers are labeled fixtures.
use super::*;
use crate::host_policy::{Approval, Change, Sharing};
use asupersync::{runtime::RuntimeBuilder, types::CancelKind};
use std::{
    fs, os::unix::fs::PermissionsExt, path::PathBuf, sync::atomic::AtomicUsize, time::Instant,
};

static SERIAL: Mutex<()> = Mutex::new(());
fn run(test: impl FnOnce(Cx)) {
    let _serial = SERIAL.lock().unwrap();
    let runtime = RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        test(Cx::current().unwrap());
    });
}
fn wait(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < until, "bounded test completion");
        thread::sleep(Duration::from_millis(2));
    }
}
fn active(handle: &Handle) -> Policy {
    wait(|| !matches!(handle.status(), Status::Opening));
    let Status::Active(policy) = handle.status() else {
        panic!("expected live policy: {:?}", handle.status());
    };
    policy
}
fn finish(watch: &mut Watch) {
    watch.stop();
    wait(|| watch.worker.as_ref().is_none_or(JoinHandle::is_finished));
    assert_eq!(watch.try_finish(), Some(Ok(())));
    assert_eq!(watch.try_finish(), Some(Ok(())));
}
struct Disk(PathBuf);
impl Disk {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fr-policy-live-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        Self(dir.join("policy.json"))
    }
    fn store(&self) -> Store {
        Store::new(&self.0).unwrap()
    }
    fn saved(&self) -> Policy {
        self.store()
            .update(Change::Approval(Approval::None))
            .unwrap()
            .policy
    }
}
fn state(policy: Policy) -> State {
    let mut state = State {
        current: None,
        until: OPEN_NS,
        previous: 0,
        failure: None,
    };
    state.publish(policy, 0, 1).unwrap();
    state
}
#[test]
fn every_new_revision_fences_old_leases_even_if_intermediate_restrictions_were_missed() {
    let p = Policy {
        revision: 1,
        ..Policy::default()
    };
    let mut state = state(p);
    let original = state.current.clone().unwrap();
    state.publish(Policy { revision: 3, ..p }, 10, 11).unwrap();
    assert_eq!(original.check(), Err(Error::Changed));
    assert!(!Arc::ptr_eq(&original, state.current.as_ref().unwrap()));
    state.stop(Error::Closed);
    assert_eq!(
        original.check(),
        Err(Error::Changed),
        "original terminal cause remains stable"
    );
}
#[test]
fn identical_read_keeps_epoch_but_expired_or_backward_clock_cannot_renew_it() {
    let p = Policy {
        revision: 1,
        ..Policy::default()
    };
    let mut s = state(p);
    let epoch = s.current.clone().unwrap();
    s.publish(p, 100, 101).unwrap();
    assert!(Arc::ptr_eq(&epoch, s.current.as_ref().unwrap()));
    assert_eq!(
        s.publish(p, VALID_NS + 100, VALID_NS + 101),
        Err(Error::Expired)
    );
    assert_eq!(epoch.check(), Err(Error::Expired));
    assert_eq!(s.publish(p, 200, 201), Err(Error::Expired));
    let mut s = state(p);
    assert_eq!(s.check(Ok(0)), Err(Error::Clock));
    assert_eq!(s.publish(p, 10, 11), Err(Error::Clock));
}
#[test]
fn rollback_same_revision_edits_and_slow_results_are_terminal() {
    let p = Policy {
        revision: 2,
        ..Policy::default()
    };
    for (new, expected) in [
        (Policy { revision: 1, ..p }, Error::Rollback),
        (
            Policy {
                approval_mode: Approval::Local,
                ..p
            },
            Error::RevisionConflict,
        ),
        (Policy::default(), Error::Rollback),
    ] {
        let mut s = state(p);
        let epoch = s.current.clone().unwrap();
        assert_eq!(s.publish(new, 10, 11), Err(expected));
        assert_eq!(epoch.check(), Err(expected));
    }
    let mut s = State {
        current: None,
        until: OPEN_NS,
        previous: 0,
        failure: None,
    };
    assert_eq!(s.publish(p, 0, VALID_NS), Err(Error::Expired));
    assert!(s.current.is_none());
}
#[test]
fn real_store_revisions_revoke_old_connections_and_new_leases_read_latest_scope() {
    run(|cx| {
        let disk = Disk::new();
        let initial = disk.saved();
        let mut watch = Watch::start(&cx, disk.store()).unwrap();
        let h = watch.handle();
        assert_eq!(active(&h), initial);
        let old = h.lease().unwrap();
        let old_clone = old.clone();
        let saved = disk
            .store()
            .update(Change::Approval(Approval::Local))
            .unwrap()
            .policy;
        wait(|| h.status() == Status::Active(saved));
        assert_eq!(old.check(), Err(Error::Changed));
        assert_eq!(old_clone.check(), Err(Error::Changed));
        let local = h.lease().unwrap();
        assert_eq!(local.check(), Ok(saved));
        let saved = disk
            .store()
            .update(Change::Sharing(Sharing::Tailnet))
            .unwrap()
            .policy;
        wait(|| h.status() == Status::Active(saved));
        assert_eq!(local.check(), Err(Error::Changed));
        assert_eq!(
            h.lease().unwrap().check().unwrap().sharing_scope,
            Sharing::Tailnet
        );
        finish(&mut watch);
        assert!(!cx.is_cancel_requested());
        assert_eq!(old.check(), Err(Error::Changed));
    });
}
#[test]
fn unchanged_disk_evidence_renews_original_lease_without_sliding_from_consumer_checks() {
    run(|cx| {
        let disk = Disk::new();
        let p = disk.saved();
        let mut watch = Watch::start(&cx, disk.store()).unwrap();
        let h = watch.handle();
        active(&h);
        let lease = h.lease().unwrap();
        thread::sleep(Duration::from_millis(750));
        assert_eq!(lease.check(), Ok(p));
        assert!(Arc::ptr_eq(&lease.epoch, &h.lease().unwrap().epoch));
        finish(&mut watch);
    });
}
#[test]
fn missing_file_defaults_are_read_only_and_a_later_save_is_a_new_epoch() {
    run(|cx| {
        let disk = Disk::new();
        let mut watch = Watch::start(&cx, disk.store()).unwrap();
        let h = watch.handle();
        assert_eq!(active(&h), Policy::default());
        assert!(!disk.0.exists());
        let lease = h.lease().unwrap();
        let saved = disk.saved();
        wait(|| h.status() == Status::Active(saved));
        assert_eq!(lease.check(), Err(Error::Changed));
        finish(&mut watch);
    });
}
#[test]
fn deleting_saved_policy_is_not_permission_to_restore_defaults() {
    run(|cx| {
        let disk = Disk::new();
        disk.saved();
        let mut watch = Watch::start(&cx, disk.store()).unwrap();
        let h = watch.handle();
        active(&h);
        let lease = h.lease().unwrap();
        fs::rename(&disk.0, disk.0.with_extension("retained")).unwrap();
        wait(|| matches!(h.status(), Status::Stopped(_)));
        assert_eq!(h.status(), Status::Stopped(Error::Rollback));
        assert_eq!(lease.check(), Err(Error::Rollback));
        disk.saved();
        thread::sleep(REFRESH * 2);
        assert_eq!(h.status(), Status::Stopped(Error::Rollback));
        finish(&mut watch);
    });
}
#[test]
fn malformed_and_symlink_substitution_refuse_and_never_recover_after_repair() {
    run(|cx| {
        for symlink in [false, true] {
            let disk = Disk::new();
            let p = disk.saved();
            let mut watch = Watch::start(&cx, disk.store()).unwrap();
            let h = watch.handle();
            active(&h);
            let lease = h.lease().unwrap();
            if symlink {
                let retained = disk.0.with_extension("original");
                fs::rename(&disk.0, &retained).unwrap();
                std::os::unix::fs::symlink(retained, &disk.0).unwrap();
            } else {
                fs::write(&disk.0, b"{broken").unwrap();
            }
            wait(|| matches!(h.status(), Status::Stopped(_)));
            let status = h.status();
            assert!(matches!(status, Status::Stopped(Error::Store(_))));
            assert!(matches!(lease.check(), Err(Error::Store(_))));
            if !symlink {
                fs::write(&disk.0, serde_json::to_vec(&p).unwrap()).unwrap();
            }
            assert_eq!(h.status(), status);
            finish(&mut watch);
        }
    });
}
#[test]
fn stopped_or_dropped_watch_retires_clones_without_cancelling_broker() {
    run(|cx| {
        let disk = Disk::new();
        let mut watch = Watch::start(&cx, disk.store()).unwrap();
        let h = watch.handle();
        active(&h);
        let lease = h.lease().unwrap();
        let copy = lease.clone();
        watch.stop();
        assert_eq!(lease.check(), Err(Error::Closed));
        assert_eq!(copy.check(), Err(Error::Closed));
        assert!(!cx.is_cancel_requested());
        finish(&mut watch);
        drop(watch);
        let watch = Watch::start(&cx, disk.store()).unwrap();
        let h = watch.handle();
        active(&h);
        let lease = h.lease().unwrap();
        let shared = watch.shared.clone();
        drop(watch);
        assert_eq!(lease.check(), Err(Error::Closed));
        assert_eq!(h.status(), Status::Stopped(Error::Closed));
        assert!(!cx.is_cancel_requested());
        drop(shared);
        wait(|| !WORKER.load(Ordering::Acquire));
    });
}
#[test]
fn stalled_read_expires_leases_and_keeps_worker_permit_until_real_exit() {
    run(|cx| {
        let disk = Disk::new();
        let p = disk.saved();
        let blocked = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let (b, r) = (blocked.clone(), release.clone());
        let mut calls = 0;
        let mut watch = Watch::start_reader(&cx, move || {
            calls += 1;
            if calls > 1 {
                b.store(true, Ordering::Release);
                while !r.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(2));
                }
            }
            Ok(p)
        })
        .unwrap();
        let h = watch.handle();
        active(&h);
        let lease = h.lease().unwrap();
        wait(|| blocked.load(Ordering::Acquire));
        thread::sleep(Duration::from_millis(550));
        assert_eq!(lease.check(), Err(Error::Expired));
        assert_eq!(h.status(), Status::Stopped(Error::Expired));
        watch.stop();
        assert_eq!(watch.try_finish(), None);
        assert!(matches!(Watch::start(&cx, disk.store()), Err(Error::Busy)));
        release.store(true, Ordering::Release);
        finish(&mut watch);
        assert_eq!(lease.check(), Err(Error::Expired));
        let mut replacement = Watch::start(&cx, disk.store()).unwrap();
        active(&replacement.handle());
        assert_eq!(lease.check(), Err(Error::Expired));
        finish(&mut replacement);
    });
}
#[test]
fn cancellation_and_reader_panic_have_terminal_receipts_and_observed_cleanup() {
    run(|cx| {
        let disk = Disk::new();
        let mut watch = Watch::start(&cx, disk.store()).unwrap();
        let h = watch.handle();
        active(&h);
        let lease = h.lease().unwrap();
        cx.cancel_fast(CancelKind::User);
        assert_eq!(lease.check(), Err(Error::Cancelled));
        finish(&mut watch);
        assert_eq!(h.status(), Status::Stopped(Error::Cancelled));
    });
    run(|cx| {
        let mut watch = Watch::start_reader(&cx, || panic!("disk-reader fixture")).unwrap();
        let h = watch.handle();
        wait(|| matches!(h.status(), Status::Stopped(_)));
        assert_eq!(h.status(), Status::Stopped(Error::Worker));
        wait(|| watch.worker.as_ref().unwrap().is_finished());
        assert_eq!(watch.try_finish(), Some(Err(Error::Worker)));
        assert!(!cx.is_cancel_requested());
    });
}

#[test]
fn process_overrides_preserve_revision_fencing_without_mutating_saved_policy() {
    run(|cx| {
        let disk = Disk::new();
        let saved = disk.saved();
        let mut watch = Watch::start(&cx, disk.store()).unwrap();
        let raw = watch.handle();
        active(&raw);
        let effective = raw
            .clone()
            .with_overrides(Some(Approval::Local), Some(Sharing::Tailnet));
        let expected = Policy {
            approval_mode: Approval::Local,
            sharing_scope: Sharing::Tailnet,
            ..saved
        };
        assert_eq!(effective.status(), Status::Active(expected));
        let lease = effective.lease().unwrap();
        assert_eq!(lease.check(), Ok(expected));
        assert_eq!(raw.lease().unwrap().check(), Ok(saved));
        assert_eq!(disk.store().load().unwrap(), saved);
        // Deriving a different local handle cannot alter an existing selection.
        let reset = effective.clone().with_overrides(None, None);
        assert_eq!(reset.lease().unwrap().check(), Ok(saved));
        assert_eq!(lease.check(), Ok(expected));
        let next = disk
            .store()
            .update(Change::Approval(Approval::Local))
            .unwrap()
            .policy;
        wait(|| raw.status() == Status::Active(next));
        // Effective values did not change, but the saved epoch did.
        assert_eq!(lease.check(), Err(Error::Changed));
        assert_eq!(
            effective.lease().unwrap().check(),
            Ok(Policy {
                revision: next.revision,
                ..expected
            })
        );
        finish(&mut watch);
        assert_eq!(lease.check(), Err(Error::Changed));
        assert!(!cx.is_cancel_requested());
    });
}
