//! The controller consumes expiry through its original maintenance projection.
use super::*;
use fr_client::clipboard::NativeError;
use fr_core::clipboard::{ClipboardSink, Endpoint, PlatformError, Publication, Stamp};
use fr_wire::clipboard::{
    Body, Message, Role, encode,
    receive::ExpiredRecord,
    session::{
        SessionError,
        synchronize::{NativeChanges, NativeClipboard, NativeText, Received},
    },
};
use std::{cell::RefCell, rc::Rc};

#[derive(Default)]
struct Calls {
    watches: usize,
    changes: usize,
    reads: usize,
    prepared: usize,
    published: usize,
    cancelled: usize,
    suspended: usize,
    closed: usize,
    text: String,
}
impl Calls {
    fn counts(&self) -> [usize; 8] {
        [
            self.watches,
            self.changes,
            self.reads,
            self.prepared,
            self.published,
            self.cancelled,
            self.suspended,
            self.closed,
        ]
    }
}
struct Native(Rc<RefCell<Calls>>);
struct Text;
impl NativeText for Text {
    fn text(&self) -> &'static str {
        ""
    }
    fn origin(&self) -> Option<Stamp> {
        None
    }
}
impl ClipboardSink for Native {
    fn prepare(&mut self, _: &str, _: Stamp) -> Result<(), PlatformError> {
        self.0.borrow_mut().prepared += 1;
        Ok(())
    }
    fn publish(&mut self, text: &str, _: Stamp) -> Publication {
        let mut state = self.0.borrow_mut();
        state.published += 1;
        text.clone_into(&mut state.text);
        Publication::SubmittedToOs
    }
}
impl NativeClipboard for Native {
    type Text = Text;
    type Error = ();

    fn watch(&mut self) -> Result<u64, ()> {
        self.0.borrow_mut().watches += 1;
        Ok(0)
    }
    fn changes(&mut self) -> Result<NativeChanges, ()> {
        self.0.borrow_mut().changes += 1;
        Ok(NativeChanges {
            latest: None,
            settled: true,
        })
    }
    fn revision(&self) -> u64 {
        0
    }
    fn prepare_for_revision(
        &mut self,
        text: &str,
        stamp: Stamp,
        revision: u64,
    ) -> Result<(), PlatformError> {
        assert_eq!(revision, 0);
        self.prepare(text, stamp)
    }
    fn begin_read(&mut self) -> Result<(), ()> {
        self.0.borrow_mut().reads += 1;
        Ok(())
    }
    fn poll_read(&mut self) -> Result<Option<Text>, ()> {
        Ok(None)
    }
    fn cancel_read(&mut self) {
        self.0.borrow_mut().cancelled += 1;
    }
    fn suspend(&mut self) -> Result<(), ()> {
        self.0.borrow_mut().suspended += 1;
        Ok(())
    }
    fn close(&mut self) {
        self.0.borrow_mut().closed += 1;
    }
}
fn record(sequence: u64, body: Body<'_>) -> Vec<u8> {
    let mut bytes = vec![0; 256];
    let length = encode(
        Message {
            stamp: Stamp {
                id: 100 + u128::from(sequence),
                source: Endpoint::Host,
                sequence,
            },
            body,
        },
        context(Role::Host),
        &L,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(length);
    bytes
}
fn begin() -> Body<'static> {
    Body::Begin {
        total_bytes: 3,
        chunks: 1,
    }
}

#[test]
fn paused_controller_refuses_expired_unsubmitted_begin_then_accepts_a_fresh_copy_on_same_lane() {
    let (mut input, lane) = attached();
    let state = Rc::new(RefCell::new(Calls::default()));
    let mut native = lane.into_native(Native(state.clone()));
    native
        .poll(
            &mut scratch(),
            &mut Gate::default(),
            || client(0),
            || Ok(91),
        )
        .unwrap();
    let old = record(1, begin());
    input.suspend(client(500_000));
    assert_eq!(
        native
            .receive_before(&old, || client(500_000), host_at(3_000_000))
            .unwrap(),
        Received::Deferred
    );
    let calls = state.borrow().counts();
    let expired = ExpiredRecord::decode(&old, context(Role::Host), &L).unwrap();
    assert_eq!(
        native.expire_receive(expired, client(3_000_000)).unwrap(),
        Received::Refused(SessionError::Clipboard(fr_core::clipboard::Error::Expired))
    );
    assert_eq!(state.borrow().counts(), calls);
    assert_eq!(native.retained_channel_bytes(), 0);
    assert!(!native.is_closed());

    ready(&mut input, 1, 3_200_000);
    input
        .accept_ticket(
            &ticket(1, H + 3_200_000, H + 4_200_000),
            correlation(3_200_000),
            client(3_200_000),
        )
        .unwrap();
    assert_eq!(
        native.receive(&old, || client(3_200_000)).unwrap(),
        Received::Refused(SessionError::Clipboard(fr_core::clipboard::Error::Replay))
    );
    for body in [
        begin(),
        Body::Chunk {
            index: 0,
            offset: 0,
            bytes: b"new",
        },
        Body::Commit { total_bytes: 3 },
    ] {
        native
            .receive(&record(2, body), || client(3_200_000))
            .unwrap();
    }
    let state = state.borrow();
    assert_eq!(state.text, "new");
    assert_eq!((state.watches, state.prepared, state.published), (1, 1, 1));
    assert_eq!((state.suspended, state.closed), (0, 0));
    assert!(!native.is_closed());
    assert_eq!(
        input
            .attach_clipboard(78, true, client(3_200_000))
            .unwrap_err(),
        Error::AlreadyAttached
    );
}

#[test]
fn expired_metadata_does_not_survive_original_controller_drop_or_clock_regression() {
    for regression in [false, true] {
        let (mut input, lane) = attached();
        let state = Rc::new(RefCell::new(Calls::default()));
        let mut native = lane.into_native(Native(state.clone()));
        input.suspend(client(500_000));
        native
            .poll(
                &mut scratch(),
                &mut Gate::default(),
                || client(500_001),
                || Ok(91),
            )
            .unwrap();
        let bytes = record(1, begin());
        let expired = ExpiredRecord::decode(&bytes, context(Role::Host), &L).unwrap();
        let at = if regression {
            client(500_000)
        } else {
            drop(input);
            client(500_002)
        };
        let error = native.expire_receive(expired, at).unwrap_err();
        let expected = if regression {
            Error::Clock
        } else {
            Error::Stopped
        };
        assert!(matches!(error, NativeError::Authority(reason) if reason == expected));
        assert!(native.is_closed());
        assert_eq!(state.borrow().published, 0);
    }
}
