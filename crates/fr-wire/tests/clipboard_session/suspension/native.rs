//! Real core/wire owners with an explicitly recording native selection fixture.
use super::*;
use fr_wire::clipboard::session::egress::Egress;
use fr_wire::clipboard::session::synchronize::{
    NativeChange, NativeChanges, NativeClipboard, NativeText, ReadProgress, Received, Synchronizer,
};
use std::{cell::RefCell, rc::Rc};

#[derive(Default)]
struct Selection {
    text: String,
    revision: u64,
    dirty: bool,
    origin: Option<Stamp>,
    watches: usize,
    changes: usize,
    reads: usize,
    publications: usize,
    suspends: usize,
    prepared: bool,
}
impl Selection {
    fn copy(&mut self, text: &str) {
        text.clone_into(&mut self.text);
        self.revision += 1;
        self.dirty = true;
        self.origin = None;
    }
}
struct Native(Rc<RefCell<Selection>>);
struct Text(String, Option<Stamp>);
impl NativeText for Text {
    fn text(&self) -> &str {
        &self.0
    }
    fn origin(&self) -> Option<Stamp> {
        self.1
    }
}
impl ClipboardSink for Native {
    fn prepare(&mut self, _: &str, _: Stamp) -> Result<(), PlatformError> {
        self.0.borrow_mut().prepared = true;
        Ok(())
    }
    fn publish(&mut self, text: &str, stamp: Stamp) -> Publication {
        let mut state = self.0.borrow_mut();
        state.publications += 1;
        text.clone_into(&mut state.text);
        state.origin = Some(stamp);
        Publication::SubmittedToOs
    }
}
impl NativeClipboard for Native {
    type Text = Text;
    type Error = ();

    fn watch(&mut self) -> Result<u64, ()> {
        let mut state = self.0.borrow_mut();
        state.watches += 1;
        Ok(state.revision)
    }
    fn changes(&mut self) -> Result<NativeChanges, ()> {
        let mut state = self.0.borrow_mut();
        state.changes += 1;
        let latest = std::mem::take(&mut state.dirty).then_some(NativeChange {
            revision: state.revision,
            has_selection: true,
            origin: state.origin,
        });
        Ok(NativeChanges {
            latest,
            settled: true,
        })
    }
    fn revision(&self) -> u64 {
        self.0.borrow().revision
    }
    fn prepare_for_revision(
        &mut self,
        text: &str,
        stamp: Stamp,
        revision: u64,
    ) -> Result<(), PlatformError> {
        assert_eq!(self.revision(), revision);
        self.prepare(text, stamp)
    }
    fn begin_read(&mut self) -> Result<(), ()> {
        self.0.borrow_mut().reads += 1;
        Ok(())
    }
    fn poll_read(&mut self) -> Result<Option<Text>, ()> {
        let state = self.0.borrow();
        Ok(Some(Text(state.text.clone(), state.origin)))
    }
    fn cancel_read(&mut self) {}
    fn suspend(&mut self) -> Result<(), ()> {
        let mut state = self.0.borrow_mut();
        state.suspends += 1;
        state.text.clear();
        Ok(())
    }
    fn close(&mut self) {}
}
fn native(input: &InputOwner) -> (Synchronizer<Native>, Rc<RefCell<Selection>>) {
    let state = Rc::new(RefCell::new(Selection::default()));
    (
        Synchronizer::new(session(input, Role::Host), Native(state.clone())),
        state,
    )
}
fn poll(native: &mut Synchronizer<Native>, gate: &mut Gate, now: u64) {
    native
        .poll(&mut [0; 1024], gate, || at(now), || Ok(42))
        .unwrap();
}

#[derive(Default)]
struct Handoff {
    gate: Gate,
    permit: Option<Egress>,
}
impl RecordSink for Handoff {
    fn try_send(&mut self, _: &[u8]) -> Result<Admission, TransportFailure> {
        Err(TransportFailure)
    }
    fn try_send_checked(
        &mut self,
        bytes: &[u8],
        permit: Egress,
    ) -> Result<Admission, TransportFailure> {
        self.permit = Some(permit);
        self.gate.try_send(bytes)
    }
}
fn queue_original_copy(
    native: &mut Synchronizer<Native>,
    state: &RefCell<Selection>,
    complete: bool,
) -> Handoff {
    state.borrow_mut().copy("original");
    let mut handoff = Handoff::default();
    let records = if complete { 3 } else { 1 };
    for _ in 0..records {
        native
            .poll(&mut [0; 1024], &mut handoff, || at(10_000), || Ok(41))
            .unwrap();
    }
    handoff
}

#[test]
fn new_revision_fences_handed_off_payload_before_the_post_native_pause_check() {
    for first_sample in [99_999, 100_000] {
        let (shared, input) = shared_owner();
        let (mut native, state) = native(&input);
        let mut handoff = queue_original_copy(&mut native, &state, false);
        let old = decode(
            &handoff.gate.last,
            context(Role::Host),
            &ProtocolLimits::ABSOLUTE,
        )
        .unwrap()
        .stamp;
        let permit = handoff.permit.clone().unwrap();
        let deadline = permit.deadline();
        assert_eq!(permit.check_operation(at(10_000)), Ok(()));
        state.borrow_mut().copy("intermediate");
        let mut clocks = 0;
        let progress = native
            .poll(
                &mut [0; 1024],
                &mut handoff,
                || {
                    clocks += 1;
                    if clocks == 1 {
                        return at(first_sample);
                    }
                    // The native notification is consumed, but the second
                    // authority read has not run: queued bytes must be fenced.
                    assert_eq!(
                        permit.check_operation(at(100_000)),
                        Err(Error::LocalChanged.into())
                    );
                    at(100_000)
                },
                || Ok(42),
            )
            .unwrap();
        assert_eq!(clocks, 2);
        assert!(matches!(progress.read, ReadProgress::Suspended));
        assert_eq!(progress.send, Pump::Suspended);
        assert_eq!(handoff.gate.calls, 1);
        assert_eq!(native.retained_channel_bytes(), 0);
        state.borrow_mut().copy("latest");
        poll(&mut native, &mut handoff.gate, 150_000);
        assert_eq!(handoff.gate.calls, 1);
        assert_eq!(state.borrow().reads, 1);
        assert!(!state.borrow().dirty);

        resume(&shared, &input);
        poll(&mut native, &mut handoff.gate, 200_000);
        let cancel = decode(
            &handoff.gate.last,
            context(Role::Host),
            &ProtocolLimits::ABSOLUTE,
        )
        .unwrap();
        assert_eq!(cancel.stamp, old);
        assert!(matches!(
            cancel.body,
            Body::Cancel(CancelReason::Superseded)
        ));
        poll(&mut native, &mut handoff.gate, 200_000);
        poll(&mut native, &mut handoff.gate, 200_000);
        let chunk = decode(
            &handoff.gate.last,
            context(Role::Host),
            &ProtocolLimits::ABSOLUTE,
        )
        .unwrap();
        assert!(matches!(
            chunk.body,
            Body::Chunk {
                bytes: b"latest",
                ..
            }
        ));
        assert_ne!(chunk.stamp, old);
        assert_eq!(permit.check(at(200_000)), Err(Error::LocalChanged.into()));
        assert_eq!(permit.deadline(), deadline);
        let state = state.borrow();
        assert_eq!(
            (
                state.watches,
                state.reads,
                state.publications,
                state.suspends
            ),
            (1, 2, 0, 0)
        );
        assert!(!native.is_closed());
    }
}

#[test]
fn a_new_paused_revision_fences_a_queued_commit_before_authority_recovers() {
    let (shared, input) = shared_owner();
    let (mut native, state) = native(&input);
    let mut handoff = queue_original_copy(&mut native, &state, true);
    let commit = decode(
        &handoff.gate.last,
        context(Role::Host),
        &ProtocolLimits::ABSOLUTE,
    )
    .unwrap();
    assert!(matches!(commit.body, Body::Commit { .. }));
    assert_eq!(native.retained_channel_bytes(), 0);
    let permit = handoff.permit.clone().unwrap();
    let deadline = permit.deadline();
    poll(&mut native, &mut handoff.gate, 100_000);
    assert_eq!(permit.check_operation(at(100_000)), Ok(()));
    state.borrow_mut().copy("latest");
    let mut clocks = 0;
    let progress = native
        .poll(
            &mut [0; 1024],
            &mut handoff,
            || {
                clocks += 1;
                if clocks == 1 {
                    return at(150_000);
                }
                if clocks == 2 {
                    assert_eq!(
                        permit.check_operation(at(200_000)),
                        Err(Error::LocalChanged.into())
                    );
                    resume(&shared, &input);
                    assert_eq!(permit.check(at(200_000)), Err(Error::LocalChanged.into()));
                }
                at(200_000)
            },
            || Ok(42),
        )
        .unwrap();
    assert!(matches!(progress.read, ReadProgress::Queued(_)));
    assert_eq!(progress.send, Pump::RecordAccepted);
    assert_eq!(handoff.gate.calls, 4);
    assert_eq!(permit.deadline(), deadline);
    poll(&mut native, &mut handoff.gate, 200_000);
    let chunk = decode(
        &handoff.gate.last,
        context(Role::Host),
        &ProtocolLimits::ABSOLUTE,
    )
    .unwrap();
    assert!(matches!(
        chunk.body,
        Body::Chunk {
            bytes: b"latest",
            ..
        }
    ));
    let state = state.borrow();
    assert_eq!(
        (
            state.watches,
            state.reads,
            state.publications,
            state.suspends
        ),
        (1, 2, 0, 0)
    );
    assert!(!native.is_closed());
}

#[test]
fn deferred_publication_echo_does_not_read_or_invalidate_an_incoming_copy() {
    let (shared, input) = shared_owner();
    let mut source = session(&input, Role::Controller);
    let (mut native, state) = native(&input);
    let mut remote = Gate::default();
    let published = stamp(source.offer(9, "published", None, at(0)).unwrap());
    for _ in 0..3 {
        pump(&mut source, &mut remote, 0);
        native.receive(&remote.last, || at(0)).unwrap();
    }
    let incoming = stamp(source.offer(10, "incoming", None, at(0)).unwrap());
    for _ in 0..2 {
        assert_eq!(pump(&mut source, &mut remote, 0), Pump::RecordAccepted);
        native.receive(&remote.last, || at(0)).unwrap();
    }
    {
        // The native notification for our publication arrives after the next
        // remote Begin. Its provenance still identifies the exact own echo.
        let mut state = state.borrow_mut();
        assert_eq!(state.origin, Some(published));
        state.revision += 1;
        state.dirty = true;
    }
    let mut outgoing = Gate::default();
    poll(&mut native, &mut outgoing, 100_000);
    assert!(!state.borrow().dirty);
    resume(&shared, &input);
    poll(&mut native, &mut outgoing, 200_000);
    assert_eq!(outgoing.calls, 0);
    assert_eq!(state.borrow().reads, 0);
    assert_eq!(
        pump(&mut source, &mut remote, 200_000),
        Pump::ItemAccepted(incoming)
    );
    let receipt = native.receive(&remote.last, || at(200_000)).unwrap();
    assert!(
        matches!(receipt, Received::Consumed(Some(receipt)) if receipt.publication == Publication::SubmittedToOs)
    );
    let state = state.borrow();
    assert_eq!(state.text, "incoming");
    assert_eq!(
        (
            state.watches,
            state.reads,
            state.publications,
            state.suspends
        ),
        (1, 0, 2, 0)
    );
    assert!(!native.is_closed());
}

#[test]
fn paused_native_owner_keeps_selection_and_sends_only_latest_copy_after_resume() {
    let (shared, input) = shared_owner();
    let (mut native, state) = native(&input);
    let mut gate = Gate::default();
    poll(&mut native, &mut gate, 0);
    state.borrow_mut().copy("copy during pause");
    let changes = state.borrow().changes;
    poll(&mut native, &mut gate, 100_000);
    {
        let state = state.borrow();
        assert_eq!(state.text, "copy during pause");
        assert_eq!(state.changes, changes + 1);
        assert_eq!((state.reads, state.publications, state.suspends), (0, 0, 0));
    }
    assert_eq!(gate.calls, 0);
    state.borrow_mut().copy("latest");
    poll(&mut native, &mut gate, 150_000);
    {
        let state = state.borrow();
        assert_eq!(state.changes, changes + 2);
        assert!(!state.dirty, "metadata was retained for resumption");
        assert_eq!((state.reads, state.publications, state.suspends), (0, 0, 0));
    }
    assert_eq!(gate.calls, 0);
    resume(&shared, &input);
    poll(&mut native, &mut gate, 200_000);
    let begin = decode(&gate.last, context(Role::Host), &ProtocolLimits::ABSOLUTE).unwrap();
    assert!(matches!(begin.body, Body::Begin { total_bytes: 6, .. }));
    poll(&mut native, &mut gate, 200_000);
    let chunk = decode(&gate.last, context(Role::Host), &ProtocolLimits::ABSOLUTE).unwrap();
    assert!(matches!(
        chunk.body,
        Body::Chunk {
            bytes: b"latest",
            ..
        }
    ));
    let state = state.borrow();
    assert_eq!((state.watches, state.reads, state.suspends), (1, 1, 0));
    assert!(!native.is_closed());
}

#[test]
fn pause_after_native_change_read_does_not_lose_that_copy() {
    let (shared, input) = shared_owner();
    let (mut native, state) = native(&input);
    let mut gate = Gate::default();
    poll(&mut native, &mut gate, 0);
    state.borrow_mut().copy("racing");
    let mut clocks = 0;
    let progress = native
        .poll(
            &mut [0; 1024],
            &mut gate,
            || {
                clocks += 1;
                at(if clocks == 1 { 99_999 } else { 100_000 })
            },
            || Ok(42),
        )
        .unwrap();
    assert!(matches!(progress.read, ReadProgress::Suspended));
    assert_eq!(progress.send, Pump::Suspended);
    assert!(!state.borrow().dirty);
    assert_eq!(state.borrow().reads, 0);
    assert_eq!(gate.calls, 0);
    resume(&shared, &input);
    poll(&mut native, &mut gate, 200_000);
    poll(&mut native, &mut gate, 200_000);
    let chunk = decode(&gate.last, context(Role::Host), &ProtocolLimits::ABSOLUTE).unwrap();
    assert!(matches!(
        chunk.body,
        Body::Chunk {
            bytes: b"racing",
            ..
        }
    ));
    assert_eq!(state.borrow().reads, 1);
    assert!(!native.is_closed());
}

#[test]
fn pause_before_read_admission_keeps_the_unread_candidate() {
    let (shared, input) = shared_owner();
    let (mut native, state) = native(&input);
    let mut gate = Gate::default();
    poll(&mut native, &mut gate, 0);
    state.borrow_mut().copy("unread");
    let mut clocks = 0;
    let progress = native
        .poll(
            &mut [0; 1024],
            &mut gate,
            || {
                clocks += 1;
                at(if clocks < 3 { 99_999 } else { 100_000 })
            },
            || Ok(42),
        )
        .unwrap();
    assert!(matches!(progress.read, ReadProgress::Suspended));
    assert_eq!(state.borrow().reads, 0);
    assert_eq!(gate.calls, 0);
    resume(&shared, &input);
    poll(&mut native, &mut gate, 200_000);
    poll(&mut native, &mut gate, 200_000);
    let chunk = decode(&gate.last, context(Role::Host), &ProtocolLimits::ABSOLUTE).unwrap();
    assert!(matches!(
        chunk.body,
        Body::Chunk {
            bytes: b"unread",
            ..
        }
    ));
    assert_eq!(state.borrow().reads, 1);
}

#[test]
fn unconsumed_receive_waits_but_prepared_commit_is_never_retried() {
    let (shared, input) = shared_owner();
    let mut source = session(&input, Role::Controller);
    let (mut native, state) = native(&input);
    source.offer(9, "wire", None, at(0)).unwrap();
    let mut gate = Gate::default();
    assert_eq!(pump(&mut source, &mut gate, 0), Pump::RecordAccepted);
    assert_eq!(
        native.receive(&gate.last, || at(100_000)).unwrap(),
        Received::Deferred
    );
    assert_eq!(state.borrow().watches, 0);
    resume(&shared, &input);
    assert_eq!(
        native.receive(&gate.last, || at(200_000)).unwrap(),
        Received::Consumed(None)
    );
    assert_eq!(pump(&mut source, &mut gate, 200_000), Pump::RecordAccepted);
    native.receive(&gate.last, || at(200_000)).unwrap();
    assert!(matches!(
        pump(&mut source, &mut gate, 200_000),
        Pump::ItemAccepted(_)
    ));
    assert_eq!(
        native
            .receive(&gate.last, || {
                at(if state.borrow().prepared {
                    500_000
                } else {
                    200_000
                })
            })
            .unwrap(),
        Received::Refused(SessionError::Clipboard(Error::Suspended))
    );
    assert_eq!(state.borrow().publications, 0);
    assert!(!native.is_closed());
    input.monitor().confirm_suspension_release().unwrap();
    shared
        .lock()
        .unwrap()
        .mark_view_ready_until(at(900_000), at(600_000))
        .unwrap();
    assert_eq!(
        native.receive(&gate.last, || at(600_000)).unwrap(),
        Received::Refused(SessionError::Clipboard(Error::UnknownTransfer))
    );
    assert_eq!(state.borrow().publications, 0);
}
