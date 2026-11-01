use super::*;
use std::{cell::RefCell, sync::mpsc};

fn shared(started: Instant, mapped: bool) -> Arc<Shared> {
    Arc::new(Shared {
        started,
        state: Mutex::new(State {
            status: if mapped {
                Status::Mapped
            } else {
                Status::Opening
            },
            mapped,
            window: None,
            choice: None,
            joined: false,
        }),
    })
}

fn complete(control: &Control) {
    let deadline = Instant::now().checked_add(Duration::from_secs(2)).unwrap();
    while !control.is_complete() {
        assert!(Instant::now() < deadline, "original worker did not join");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn rows_and_failures_do_not_disclose_identity() {
    let row = Row::new(
        "private-machine.tail.example",
        "private-node-id",
        PathHint::Relayed,
    )
    .unwrap();
    let debug = format!("{row:?}");
    assert!(!debug.contains("private"));
    assert_eq!(
        Row::new("", "id", PathHint::Unknown).unwrap_err(),
        Error::InvalidRow
    );
    assert_eq!(
        Row::new("a\nb", "id", PathHint::Unknown).unwrap_err(),
        Error::InvalidRow
    );
    assert_eq!(
        Row::new("host.test", "a b", PathHint::Unknown).unwrap_err(),
        Error::InvalidRow
    );
    assert_eq!(
        Row::new(&"x".repeat(255), "id", PathHint::Unknown).unwrap_err(),
        Error::InvalidRow
    );
    assert_eq!(
        Row::new("host.test", &"i".repeat(129), PathHint::Unknown).unwrap_err(),
        Error::InvalidRow
    );
    assert!(Row::new(&"x".repeat(254), &"i".repeat(128), PathHint::Unknown).is_ok());
}

#[test]
fn invalid_or_ambiguous_snapshots_refuse_before_a_worker_starts() {
    let row = Row::new("host.test", "node", PathHint::Unknown).unwrap();
    assert_eq!(validate(":0", &[]), Err(Error::Empty));
    assert_eq!(
        validate(":0", &vec![row.clone(); MAX_ROWS + 1]),
        Err(Error::Capacity)
    );
    assert_eq!(
        validate(":0", &[row.clone(), row.clone()]),
        Err(Error::DuplicateIdentity)
    );
    let alias = Row::new("different.test", "node", PathHint::Direct).unwrap();
    assert_eq!(
        validate(":0", &[row.clone(), alias]),
        Err(Error::DuplicateIdentity)
    );
    let alias = Row::new("host.test", "other-node", PathHint::Direct).unwrap();
    assert_eq!(
        validate(":0", &[row.clone(), alias]),
        Err(Error::DuplicateIdentity)
    );
    for display in ["", "remote:0", ":", ":0.0.0", ":123456", ":0\0"] {
        assert_eq!(
            validate(display, std::slice::from_ref(&row)),
            Err(Error::InvalidDisplay)
        );
    }
    assert_eq!(validate(":0.0", &[row]), Ok(()));
}

#[test]
fn absolute_map_and_choice_expiry_cannot_claim_cleanup_or_release_late_choice() {
    for (age, mapped, error) in [
        (MAP_LIMIT, false, Error::MapTimeout),
        (CHOICE_LIMIT, true, Error::ChoiceTimeout),
    ] {
        let owner = shared(Instant::now().checked_sub(age).unwrap(), mapped);
        let control = Control(Arc::clone(&owner));
        assert_eq!(control.status(), Status::Failed(error));
        assert!(!control.is_complete());
        owner.finish(Ok(Some(0)));
        owner.joined(false);
        let mut picker = HostPicker { control };
        assert_eq!(picker.poll(), Err(error));
        assert!(picker.is_complete());
    }
}

#[test]
fn joined_but_unconsumed_choice_keeps_the_original_absolute_deadline() {
    let mut owner = shared(Instant::now(), true);
    owner.finish(Ok(Some(17)));
    owner.joined(false);
    assert_eq!(owner.state().status, Status::Chosen);
    assert!(owner.state().joined);

    // Advance the original start instant only; neither a successful join nor
    // the eventual caller poll creates a new choice lifetime.
    Arc::get_mut(&mut owner).unwrap().started = Instant::now().checked_sub(CHOICE_LIMIT).unwrap();
    let mut picker = HostPicker {
        control: Control(owner),
    };
    assert_eq!(picker.poll(), Err(Error::ChoiceTimeout));
    assert!(picker.is_complete());
    assert_eq!(picker.control.0.state().choice, None);
    picker.cancel();
    assert_eq!(picker.status(), Status::Failed(Error::ChoiceTimeout));
}

struct LocalDestructor {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}
impl Drop for LocalDestructor {
    fn drop(&mut self) {
        let _ = self.entered.send(());
        let _ = self.release.recv();
    }
}
thread_local! {
    static LOCAL: RefCell<Option<LocalDestructor>> = const { RefCell::new(None) };
}

fn delayed_destructor() -> (HostPicker, mpsc::Receiver<()>, mpsc::SyncSender<()>) {
    let shared = shared(Instant::now(), true);
    let (entered, entered_rx) = mpsc::sync_channel(1);
    let (release, release_rx) = mpsc::sync_channel(1);
    supervise(Arc::clone(&shared), move || {
        LOCAL.with(|local| {
            *local.borrow_mut() = Some(LocalDestructor {
                entered,
                release: release_rx,
            });
        });
        Ok(Some(17))
    })
    .unwrap();
    (
        HostPicker {
            control: Control(shared),
        },
        entered_rx,
        release,
    )
}

#[test]
fn choice_waits_for_actual_join_including_thread_local_destructors() {
    let (mut picker, entered, release) = delayed_destructor();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(picker.status(), Status::Closing);
    assert_eq!(picker.poll(), Ok(None));
    // In particular this call must not use is_finished() followed by join().
    assert!(!picker.is_complete());
    release.send(()).unwrap();
    complete(&picker.control());
    assert_eq!(picker.status(), Status::Chosen);
    assert_eq!(picker.poll(), Ok(Some(17)));
    assert_eq!(picker.poll(), Ok(None));
    assert_eq!(picker.status(), Status::Consumed);
}

#[test]
fn cancel_during_native_cleanup_discards_the_unpublished_original_choice() {
    let (mut picker, entered, release) = delayed_destructor();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    picker.control().cancel();
    assert_eq!(picker.status(), Status::Cancelled);
    assert!(!picker.is_complete());
    release.send(()).unwrap();
    complete(&picker.control());
    assert_eq!(picker.poll(), Ok(None));
    assert_eq!(picker.status(), Status::Cancelled);
}

#[test]
fn dropped_picker_cancels_without_joining_on_the_calling_thread() {
    let (picker, entered, release) = delayed_destructor();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    let control = picker.control();
    drop(picker);
    assert_eq!(control.status(), Status::Cancelled);
    assert!(!control.is_complete());
    release.send(()).unwrap();
    complete(&control);
    assert_eq!(control.status(), Status::Cancelled);
}

#[test]
fn native_worker_panic_never_releases_a_choice() {
    let shared = shared(Instant::now(), true);
    supervise(Arc::clone(&shared), || {
        panic!("deliberate native worker failure")
    })
    .unwrap();
    let mut picker = HostPicker {
        control: Control(shared),
    };
    complete(&picker.control());
    assert_eq!(picker.poll(), Err(Error::WorkerPanicked));
}
