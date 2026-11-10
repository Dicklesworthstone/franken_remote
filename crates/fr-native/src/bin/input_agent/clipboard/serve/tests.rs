//! Exercise the actual framed dispatcher through private sockets. The native
//! owner below is an explicit test fixture, not platform qualification.
use super::*;
use fr_core::clipboard::{Endpoint, PlatformError};
use fr_wire::clipboard::session::synchronize::NativeChanges;
use std::{
    io::Write,
    net::Shutdown,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

struct Text(Vec<u8>);
impl NativeText for Text {
    fn text(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap()
    }
    fn origin(&self) -> Option<Stamp> {
        None
    }
}
impl Drop for Text {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

struct Fixture {
    live: Arc<AtomicBool>,
    revision: u64,
    prepared: Option<(String, Stamp)>,
    preparations: usize,
    submissions: usize,
    reads: usize,
    deadline: Option<u64>,
    stop_on_read: bool,
    publication: Publication,
    closed: bool,
}
impl Fixture {
    fn new(live: Arc<AtomicBool>) -> Self {
        Self {
            live,
            revision: 1,
            prepared: None,
            preparations: 0,
            submissions: 0,
            reads: 0,
            deadline: None,
            stop_on_read: false,
            publication: Publication::SubmittedToOs,
            closed: false,
        }
    }
}
impl ClipboardSink for Fixture {
    fn prepare(&mut self, text: &str, stamp: Stamp) -> Result<(), PlatformError> {
        self.preparations += 1;
        self.prepared = Some((text.to_owned(), stamp));
        Ok(())
    }
    fn publish(&mut self, _: &str, _: Stamp) -> Publication {
        panic!("the process dispatcher must use the final deadline gate")
    }
    fn cancel_prepared(&mut self) {
        self.prepared = None;
    }
}
impl NativeClipboard for Fixture {
    type Text = Text;
    type Error = Failure;

    fn watch(&mut self) -> Result<u64, Failure> {
        Ok(self.revision)
    }
    fn changes(&mut self) -> Result<NativeChanges, Failure> {
        Ok(NativeChanges {
            latest: None,
            settled: true,
        })
    }
    fn revision(&self) -> u64 {
        self.revision
    }
    fn prepare_for_revision(
        &mut self,
        text: &str,
        stamp: Stamp,
        revision: u64,
    ) -> Result<(), PlatformError> {
        if revision != self.revision {
            return Err(PlatformError::LocalChanged);
        }
        self.prepare(text, stamp)
    }
    fn begin_read(&mut self) -> Result<(), Failure> {
        Ok(())
    }
    fn poll_read(&mut self) -> Result<Option<Text>, Failure> {
        self.reads += 1;
        if self.stop_on_read {
            self.live.store(false, Ordering::Release);
        }
        Ok(Some(Text("local λ".as_bytes().to_vec())))
    }
    fn cancel_read(&mut self) {}
    fn suspend(&mut self) -> Result<(), Failure> {
        self.cancel_prepared();
        Ok(())
    }
    fn close(&mut self) {
        self.cancel_prepared();
        self.closed = true;
    }
}
impl Backend for Fixture {
    fn publish_prepared(
        &mut self,
        stamp: Stamp,
        not_after_ns: u64,
        live: &dyn Fn() -> bool,
    ) -> Publication {
        self.deadline = Some(not_after_ns);
        let Some((_, original)) = self.prepared.take() else {
            return Publication::NotSubmitted(PlatformError::Unavailable);
        };
        if stamp != original || not_after_ns <= 10 || !live() {
            return Publication::NotSubmitted(PlatformError::Unavailable);
        }
        self.submissions += 1;
        self.revision += 1;
        self.publication
    }
    fn failure(error: Failure) -> Failure {
        error
    }
}

fn stamp() -> Stamp {
    Stamp {
        id: 11,
        source: Endpoint::Controller,
        sequence: 1,
    }
}

fn preparation(revision: Option<u64>, len: u32) -> Request {
    Request::Prepare {
        stamp: stamp(),
        revision,
        len,
    }
}

fn publication(not_after_ns: u64) -> Request {
    Request::Publish {
        stamp: stamp(),
        not_after_ns,
    }
}

fn run(
    owner: &mut Fixture,
    max: u32,
    requests: &[(u64, Request, &[u8])],
) -> (i32, Vec<(Reply, Vec<u8>)>) {
    let (mut peer, mut command) = UnixStream::pair().unwrap();
    for (sequence, request, payload) in requests {
        peer.write_all(&process::encode_request(*sequence, *request).unwrap())
            .unwrap();
        peer.write_all(payload).unwrap();
    }
    peer.shutdown(Shutdown::Write).unwrap();
    let live = owner.live.clone();
    let code = serve(&mut command, owner, max, &|| live.load(Ordering::Acquire));
    drop(command);
    let mut received = Vec::new();
    loop {
        let mut frame = [0; process::FRAME_BYTES];
        match peer.read_exact(&mut frame) {
            Ok(()) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::UnexpectedEof
                    || (code != 0 && error.kind() == std::io::ErrorKind::ConnectionReset) =>
            {
                break;
            }
            Err(error) => panic!("private test channel: {error}"),
        }
        let (_, reply) = process::decode_reply(&frame, max).unwrap();
        let mut payload = match reply {
            Reply::ReadText { len, .. } => vec![0; len as usize],
            _ => Vec::new(),
        };
        peer.read_exact(&mut payload).unwrap();
        received.push((reply, payload));
    }
    (code, received)
}

#[test]
fn framed_prepare_publish_read_and_stop_preserve_original_deadline_and_close() {
    let live = Arc::new(AtomicBool::new(true));
    let mut owner = Fixture::new(live);
    let (code, replies) = run(
        &mut owner,
        32,
        &[
            (2, preparation(Some(1), 3), "λ!".as_bytes()),
            (3, publication(99), &[]),
            (4, Request::BeginRead, &[]),
            (5, Request::PollRead, &[]),
            (6, Request::Stop, &[]),
        ],
    );
    assert_eq!(code, 0);
    assert_eq!(owner.deadline, Some(99));
    assert_eq!(owner.submissions, 1);
    assert!(owner.closed);
    assert_eq!(
        replies[1].0,
        Reply::Published {
            revision: 2,
            publication: Publication::SubmittedToOs,
        }
    );
    assert_eq!(replies[3].1, "local λ".as_bytes());
    assert_eq!(replies[4].0, Reply::Stopped);
}

#[test]
fn expired_publication_is_consumed_and_cannot_get_a_fresh_deadline() {
    let mut owner = Fixture::new(Arc::new(AtomicBool::new(true)));
    let (code, replies) = run(
        &mut owner,
        32,
        &[
            (2, preparation(None, 1), b"x"),
            (3, publication(10), &[]),
            (4, publication(100), &[]),
            (5, Request::Stop, &[]),
        ],
    );
    assert_eq!(code, 0);
    assert_eq!(owner.submissions, 0);
    for index in [1, 2] {
        assert_eq!(
            replies[index].0,
            Reply::Published {
                revision: 1,
                publication: Publication::NotSubmitted(PlatformError::Unavailable),
            }
        );
    }
}

#[test]
fn malformed_or_unbounded_input_never_reaches_native_preparation() {
    for (sequence, len, payload, code) in [
        (3, 1, b"x".as_slice(), EXIT_PROTOCOL),
        (2, 9, b"".as_slice(), EXIT_PROTOCOL),
        (2, 1, b"\xff".as_slice(), EXIT_PROTOCOL),
        (2, 3, b"x".as_slice(), EXIT_CHANNEL),
    ] {
        let mut owner = Fixture::new(Arc::new(AtomicBool::new(true)));
        let (actual, replies) = run(
            &mut owner,
            8,
            &[(sequence, preparation(None, len), payload)],
        );
        assert_eq!(actual, code);
        assert_eq!(owner.preparations, 0);
        assert_eq!(replies, []);
    }
}

#[test]
fn local_stop_during_native_read_emits_no_clipboard_bytes() {
    let mut owner = Fixture::new(Arc::new(AtomicBool::new(true)));
    owner.stop_on_read = true;
    let (code, replies) = run(&mut owner, 32, &[(2, Request::PollRead, &[])]);
    assert_eq!(code, EXIT_REFUSED);
    assert_eq!(owner.reads, 1);
    assert_eq!(replies, []);
}

#[test]
fn stopped_privacy_allows_cleanup_but_no_new_native_work() {
    let mut owner = Fixture::new(Arc::new(AtomicBool::new(false)));
    let (code, replies) = run(
        &mut owner,
        32,
        &[(2, Request::CancelPrepared, &[]), (3, Request::Stop, &[])],
    );
    assert_eq!(code, 0);
    assert!(owner.closed);
    assert_eq!(replies.last().unwrap().0, Reply::Stopped);
    let mut owner = Fixture::new(Arc::new(AtomicBool::new(false)));
    let (code, replies) = run(&mut owner, 32, &[(2, Request::PollRead, &[])]);
    assert_eq!(code, EXIT_REFUSED);
    assert_eq!(owner.reads, 0);
    assert_eq!(replies, []);
}

#[test]
fn uncertain_native_publication_is_reported_without_retry() {
    let mut owner = Fixture::new(Arc::new(AtomicBool::new(true)));
    owner.publication = Publication::UnknownEffect;
    let (code, replies) = run(
        &mut owner,
        32,
        &[
            (2, preparation(None, 1), b"x"),
            (3, publication(100), &[]),
            (4, Request::Stop, &[]),
        ],
    );
    assert_eq!(code, 0);
    assert_eq!(owner.submissions, 1);
    assert_eq!(
        replies[1].0,
        Reply::Published {
            revision: 2,
            publication: Publication::UnknownEffect,
        }
    );
}
