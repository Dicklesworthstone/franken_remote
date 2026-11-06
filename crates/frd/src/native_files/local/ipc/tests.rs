//! Real Linux abstract sockets and credentials exercise the shipped local
//! command framing and worker ownership. The recording handler witnesses IPC
//! delivery only; these tests do not claim file publication or remote authority.
use super::*;
use std::{
    cell::RefCell,
    ffi::OsString,
    os::{
        linux::net::SocketAddrExt,
        unix::{
            ffi::OsStringExt,
            net::{SocketAddr, UnixListener},
        },
    },
    sync::{
        Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

static LABEL: AtomicU64 = AtomicU64::new(0);
fn target() -> String {
    format!(
        "test-{}-{}",
        std::process::id(),
        LABEL.fetch_add(1, Ordering::Relaxed)
    )
}
fn connection() -> String {
    format!(
        "{:016x}{:016x}",
        std::process::id(),
        LABEL.fetch_add(1, Ordering::Relaxed)
    )
}
#[derive(Default)]
struct Records {
    paths: Vec<Vec<PathBuf>>,
    cancelled: Vec<bool>,
    pause_requested: Vec<bool>,
}
#[derive(Default)]
struct Probe {
    records: Mutex<Records>,
    closed: AtomicBool,
    entered: AtomicBool,
    hold: Mutex<bool>,
    wake: Condvar,
    exit: Mutex<Option<Arc<Exit>>>,
    deadline: Mutex<Option<Instant>>,
}

#[derive(Default)]
struct Exit {
    entered: AtomicBool,
    release: Mutex<bool>,
    wake: Condvar,
}
struct OnThreadExit(Arc<Exit>);
impl Drop for OnThreadExit {
    fn drop(&mut self) {
        self.0.entered.store(true, Ordering::Release);
        let mut release = self.0.release.lock().unwrap();
        while !*release {
            release = self.0.wake.wait(release).unwrap();
        }
    }
}
thread_local! {
    static THREAD_EXIT: RefCell<Option<OnThreadExit>> = const { RefCell::new(None) };
}
impl Handler for Probe {
    fn status(&self, sequence: Option<u64>) -> Result<Status, Error> {
        let records = self.records.lock().unwrap();
        let sequence = sequence.unwrap_or(u64::try_from(records.paths.len()).unwrap());
        let mut status = Status {
            sequence,
            ready: !self.closed.load(Ordering::Acquire),
            closed: self.closed.load(Ordering::Acquire),
            ..Status::default()
        };
        if sequence != 0 {
            let index = usize::try_from(sequence - 1).unwrap();
            let paths = records.paths.get(index).ok_or(Error::UnknownRequest)?;
            status.selected = u16::try_from(paths.len()).unwrap();
            status.phase = Phase::WaitingForControl;
            status.cancel_requested = records.cancelled[index];
            status.pause_requested = Some(records.pause_requested[index]);
        }
        Ok(status)
    }
    fn select(
        &self,
        paths: &[PathBuf],
        until: Instant,
        stopped: &AtomicBool,
    ) -> Result<Status, Error> {
        if let Some(exit) = self.exit.lock().unwrap().as_ref() {
            THREAD_EXIT.with(|slot| *slot.borrow_mut() = Some(OnThreadExit(Arc::clone(exit))));
        }
        *self.deadline.lock().unwrap() = Some(until);
        self.entered.store(true, Ordering::Release);
        let mut hold = self.hold.lock().unwrap();
        while *hold {
            hold = self.wake.wait(hold).unwrap();
        }
        drop(hold);
        if stopped.load(Ordering::Acquire) || self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        check_time(until)?;
        let mut records = self.records.lock().unwrap();
        records.paths.push(paths.to_vec());
        records.cancelled.push(false);
        records.pause_requested.push(false);
        drop(records);
        self.status(None)
    }
    fn cancel(&self, sequence: u64) -> Result<(Status, bool), Error> {
        let mut records = self.records.lock().unwrap();
        let index = usize::try_from(sequence.checked_sub(1).ok_or(Error::InvalidRequest)?).unwrap();
        let cancelled = records
            .cancelled
            .get_mut(index)
            .ok_or(Error::UnknownRequest)?;
        let requested = !*cancelled;
        *cancelled = true;
        drop(records);
        Ok((self.status(Some(sequence))?, requested))
    }
    fn set_paused(&self, sequence: u64, paused: bool) -> Result<(Status, bool), Error> {
        let mut records = self.records.lock().unwrap();
        let index = usize::try_from(sequence.checked_sub(1).ok_or(Error::InvalidRequest)?).unwrap();
        let cancelled = *records.cancelled.get(index).ok_or(Error::UnknownRequest)?;
        let intent = &mut records.pause_requested[index];
        let changed = !cancelled && *intent != paused;
        if changed {
            *intent = paused;
        }
        drop(records);
        Ok((self.status(Some(sequence))?, changed))
    }
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}
fn owner() -> (Owner, Arc<Probe>) {
    let handler = Arc::new(Probe::default());
    let owner = Owner::bind(&target(), handler.clone()).unwrap();
    (owner, handler)
}
fn wait(mut ready: impl FnMut() -> bool) {
    let until = Instant::now().checked_add(Duration::from_secs(3)).unwrap();
    while !ready() {
        pause(until).unwrap();
    }
}
fn stop(owner: &Owner) {
    owner.stop();
    wait(|| owner.is_complete());
}
fn raw(owner: &Owner) -> (UnixStream, [u8; 16], Instant) {
    let info = owner.info().unwrap();
    let (stream, _, instance, until) = identify(Scope::Host, &info.target).unwrap();
    (stream, instance, until)
}

#[test]
fn inspect_is_content_free_and_does_not_select_or_cancel() {
    let (owner, handler) = owner();
    let initial = owner.info().unwrap();
    let found = inspect(&initial.target).unwrap();
    assert_eq!(found, initial);
    assert_eq!(found.instance.len(), 32);
    assert_eq!(handler.records.lock().unwrap().paths.len(), 0);
    assert!(!handler.closed.load(Ordering::Acquire));
    stop(&owner);
}

#[test]
fn upload_and_host_namespaces_cannot_address_each_others_selections() {
    let connection = connection();
    let host_handler = Arc::new(Probe::default());
    let upload_handler = Arc::new(Probe::default());
    let host = Owner::bind(&connection, host_handler.clone()).unwrap();
    let upload = Owner::bind_upload(&connection, upload_handler.clone()).unwrap();
    let host_info = inspect(&connection).unwrap();
    let upload_info = upload::inspect(&connection).unwrap();
    assert_eq!(host_info, host.info().unwrap());
    assert_eq!(upload_info, upload.info().unwrap());
    assert_ne!(host_info.instance, upload_info.instance);
    assert_eq!(
        send(&connection, &upload_info.instance, &["/wrong-host".into()]),
        Err(Error::WrongInstance)
    );
    assert_eq!(
        upload::send(&connection, &host_info.instance, &["/wrong-upload".into()]),
        Err(Error::WrongInstance)
    );
    let accepted = upload::send(
        &connection,
        &upload_info.instance,
        &["/explicit-upload".into()],
    )
    .unwrap();
    assert_eq!(accepted.sequence, 1);
    assert_eq!(
        host_handler.records.lock().unwrap().paths,
        Vec::<Vec<PathBuf>>::new()
    );
    assert_eq!(
        upload_handler.records.lock().unwrap().paths,
        [vec![PathBuf::from("/explicit-upload")]]
    );
    assert_eq!(
        upload::status(&connection, &upload_info.instance, Some(1))
            .unwrap()
            .status
            .sequence,
        1
    );
    assert!(
        upload::cancel(&connection, &upload_info.instance, 1)
            .unwrap()
            .requested
    );
    assert_eq!(inspect(&connection).unwrap().status.sequence, 0);
    stop(&upload);
    stop(&host);
}

#[test]
fn reused_connection_name_never_accepts_an_original_upload_lease() {
    let connection = connection();
    let first = Owner::bind_upload(&connection, Arc::new(Probe::default())).unwrap();
    let original = first.info().unwrap();
    let request = upload::send(&connection, &original.instance, &["/first".into()])
        .unwrap()
        .sequence;
    stop(&first);
    let handler = Arc::new(Probe::default());
    let replacement = Owner::bind_upload(&connection, handler.clone()).unwrap();
    assert_ne!(replacement.info().unwrap().instance, original.instance);
    assert_eq!(
        upload::send(&connection, &original.instance, &["/stale".into()]),
        Err(Error::WrongInstance)
    );
    assert_eq!(
        upload::status(&connection, &original.instance, Some(request)),
        Err(Error::WrongInstance)
    );
    assert_eq!(
        upload::cancel(&connection, &original.instance, request),
        Err(Error::WrongInstance)
    );
    assert_eq!(
        handler.records.lock().unwrap().paths,
        Vec::<Vec<PathBuf>>::new()
    );
    stop(&replacement);
}

#[test]
fn upload_targets_are_exact_foreground_connection_handles_before_any_io() {
    for invalid in [
        "host",
        "../host",
        "",
        "0123456789abcdef0123456789abcdeg",
        "0123456789abcdef0123456789abcdef0",
    ] {
        assert_eq!(upload::inspect(invalid), Err(Error::InvalidTarget));
        assert!(matches!(
            Owner::bind_upload(invalid, Arc::new(Probe::default())),
            Err(Error::InvalidTarget)
        ));
    }
    let connection = connection();
    assert_eq!(
        upload::send(&connection, &"a".repeat(32), &[]),
        Err(Error::InvalidPaths)
    );
    assert_eq!(
        upload::status(&connection, &"a".repeat(32), Some(0)),
        Err(Error::InvalidRequest)
    );
    assert_eq!(
        upload::cancel(&connection, &"a".repeat(32), 0),
        Err(Error::InvalidRequest)
    );
}

#[test]
fn complete_raw_unix_paths_are_delivered_once_and_report_only_admission() {
    let (owner, handler) = owner();
    let info = owner.info().unwrap();
    let paths = vec![
        PathBuf::from("/explicit/a"),
        PathBuf::from(OsString::from_vec(b"/explicit/non-utf8-\xff".to_vec())),
    ];
    let accepted = send(&info.target, &info.instance, &paths).unwrap();
    assert_eq!(accepted.sequence, 1);
    assert_eq!(accepted.info.status.selected, 2);
    assert_eq!(accepted.info.status.published, 0);
    assert_eq!(accepted.info.status.cleanup, Cleanup::Unknown);
    assert_eq!(handler.records.lock().unwrap().paths, vec![paths]);
    stop(&owner);
}

#[test]
fn exact_sequence_status_and_cancel_cannot_name_the_next_command() {
    let (owner, _) = owner();
    let info = owner.info().unwrap();
    let first = send(&info.target, &info.instance, &["/first".into()]).unwrap();
    let second = send(&info.target, &info.instance, &["/second".into()]).unwrap();
    assert_eq!(
        status(&info.target, &info.instance, None)
            .unwrap()
            .status
            .sequence,
        second.sequence
    );
    let cancelled = cancel(&info.target, &info.instance, first.sequence).unwrap();
    assert!(cancelled.requested);
    assert!(cancelled.info.status.cancel_requested);
    assert!(
        !cancel(&info.target, &info.instance, first.sequence)
            .unwrap()
            .requested
    );
    assert!(
        !status(&info.target, &info.instance, Some(second.sequence))
            .unwrap()
            .status
            .cancel_requested
    );
    assert_eq!(
        status(&info.target, &info.instance, Some(99)),
        Err(Error::UnknownRequest)
    );
    stop(&owner);
}

#[test]
fn replacement_instance_never_receives_a_stale_selection_or_cancellation() {
    let (original, _) = owner();
    let old = original.info().unwrap();
    stop(&original);
    let handler = Arc::new(Probe::default());
    let replacement = Owner::bind(&old.target, handler.clone()).unwrap();
    assert_ne!(replacement.info().unwrap().instance, old.instance);
    assert_eq!(
        send(&old.target, &old.instance, &["/must-not-open".into()]),
        Err(Error::WrongInstance)
    );
    assert_eq!(
        cancel(&old.target, &old.instance, 1),
        Err(Error::WrongInstance)
    );
    assert_eq!(handler.records.lock().unwrap().paths.len(), 0);
    stop(&replacement);
}

#[test]
fn kernel_credentials_are_checked_against_the_original_effective_user() {
    let (left, right) = UnixStream::pair().unwrap();
    let uid = rustix::process::geteuid().as_raw();
    assert_eq!(check_peer(&left, uid), Ok(()));
    assert_eq!(check_peer(&right, uid), Ok(()));
    let different = u32::from(uid == 0);
    assert_eq!(check_peer(&left, different), Err(Error::UntrustedPeer));
    assert_eq!(check_peer(&right, different), Err(Error::UntrustedPeer));
}

#[test]
fn four_pending_peers_do_not_admit_a_fifth_and_closure_releases_capacity() {
    let (owner, _) = owner();
    let info = owner.info().unwrap();
    let mut peers = Vec::new();
    for _ in 0..MAX_PEERS {
        peers.push(raw(&owner).0);
    }
    assert!(inspect(&info.target).is_err());
    drop(peers);
    wait(|| inspect(&info.target).is_ok());
    stop(&owner);
}

#[test]
fn trailing_bytes_and_oversized_headers_never_reach_the_selection_worker() {
    let (owner, handler) = owner();
    let (mut stream, instance, until) = raw(&owner);
    let payload = wire::paths(&["/one".into()]).unwrap();
    write(
        &mut stream,
        &wire::command(
            wire::Version::V1,
            wire::Command::Send(1),
            instance,
            payload.len(),
        )
        .unwrap(),
        until,
    )
    .unwrap();
    write(&mut stream, &payload, until).unwrap();
    write(&mut stream, b"trailing", until).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut response = [0; wire::RESULT_BYTES];
    read(&mut stream, &mut response, until).unwrap();
    assert_eq!(
        wire::result(wire::Version::V1, &response, instance),
        Err(Error::Protocol)
    );

    let (mut stream, instance, until) = raw(&owner);
    let mut header = wire::command(
        wire::Version::V1,
        wire::Command::Send(1),
        instance,
        payload.len(),
    )
    .unwrap();
    header[34..38].copy_from_slice(&u32::MAX.to_be_bytes());
    write(&mut stream, &header, until).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    read(&mut stream, &mut response, until).unwrap();
    assert_eq!(
        wire::result(wire::Version::V1, &response, instance),
        Err(Error::InvalidPaths)
    );
    assert_eq!(handler.records.lock().unwrap().paths.len(), 0);
    stop(&owner);
}

#[test]
fn command_requires_end_of_frame_and_keeps_the_original_handshake_budget() {
    let (owner, handler) = owner();
    let (mut stream, instance, until) = raw(&owner);
    let payload = wire::paths(&["/not-yet-consented".into()]).unwrap();
    write(
        &mut stream,
        &wire::command(
            wire::Version::V1,
            wire::Command::Send(1),
            instance,
            payload.len(),
        )
        .unwrap(),
        until,
    )
    .unwrap();
    write(&mut stream, &payload, until).unwrap();
    // A complete length-prefixed body without the required EOF never submits.
    // A second authenticated inspection remains independently serviceable.
    assert_eq!(
        inspect(&owner.info().unwrap().target)
            .unwrap()
            .status
            .sequence,
        0
    );
    let mut byte = [0];
    let read_until = until.checked_add(Duration::from_secs(1)).unwrap();
    assert_eq!(
        read(&mut stream, &mut byte, read_until),
        Err(Error::Protocol)
    );
    assert_eq!(handler.records.lock().unwrap().paths.len(), 0);
    assert!(Instant::now() >= until);
    stop(&owner);
}

#[test]
fn one_blocked_selection_cannot_block_status_or_admit_a_second_worker() {
    let (owner, handler) = owner();
    *handler.hold.lock().unwrap() = true;
    let info = owner.info().unwrap();
    let first_info = info.clone();
    let first = thread::spawn(move || {
        send(
            &first_info.target,
            &first_info.instance,
            &["/blocked".into()],
        )
    });
    wait(|| handler.entered.load(Ordering::Acquire));
    assert_eq!(
        status(&info.target, &info.instance, None)
            .unwrap()
            .status
            .sequence,
        0
    );
    assert_eq!(
        send(&info.target, &info.instance, &["/second".into()]),
        Err(Error::Busy)
    );
    owner.stop();
    assert!(!owner.is_complete());
    *handler.hold.lock().unwrap() = false;
    handler.wake.notify_all();
    assert_eq!(first.join().unwrap(), Err(Error::OutcomeUnknown));
    wait(|| owner.is_complete());
    assert_eq!(handler.records.lock().unwrap().paths.len(), 0);
}

#[test]
fn completion_requires_the_original_thread_and_its_local_destructors_to_join() {
    let (owner, handler) = owner();
    let exit = Arc::new(Exit::default());
    *handler.exit.lock().unwrap() = Some(Arc::clone(&exit));
    let info = owner.info().unwrap();
    send(&info.target, &info.instance, &["/selected".into()]).unwrap();
    owner.stop();
    wait(|| exit.entered.load(Ordering::Acquire));
    assert!(
        !owner.is_complete(),
        "thread-local cleanup still owns the original thread"
    );
    *exit.release.lock().unwrap() = true;
    exit.wake.notify_all();
    wait(|| owner.is_complete());
}

#[test]
fn a_stalled_selection_does_not_refresh_its_original_command_deadline() {
    let (owner, handler) = owner();
    *handler.hold.lock().unwrap() = true;
    let info = owner.info().unwrap();
    let first_info = info.clone();
    let first = thread::spawn(move || {
        send(
            &first_info.target,
            &first_info.instance,
            &["/stalled".into()],
        )
    });
    wait(|| handler.entered.load(Ordering::Acquire));
    assert_eq!(first.join().unwrap(), Err(Error::OutcomeUnknown));
    let original_until = handler.deadline.lock().unwrap().unwrap();
    wait(|| Instant::now() >= original_until);
    *handler.hold.lock().unwrap() = false;
    handler.wake.notify_all();
    // The original late selector refuses; a new explicit action can use its
    // own budget. It must become sequence1, not a replay after an unseen send.
    let until = Instant::now().checked_add(HANDSHAKE).unwrap();
    let next = loop {
        match send(
            &info.target,
            &info.instance,
            &["/fresh-explicit-selection".into()],
        ) {
            Err(Error::Busy) => pause(until).unwrap(),
            result => break result.unwrap(),
        }
    };
    assert_eq!(next.sequence, 1);
    assert_eq!(
        handler.records.lock().unwrap().paths,
        vec![vec![PathBuf::from("/fresh-explicit-selection")]]
    );
    stop(&owner);
}

#[test]
fn a_lost_or_malformed_post_command_reply_is_never_a_safe_retry() {
    let target = target();
    let uid = rustix::process::geteuid().as_raw();
    let address = SocketAddr::from_abstract_name(name(Scope::Host, uid, &target)).unwrap();
    let listener = UnixListener::bind_addr(&address).unwrap();
    let instance = [17; 16];
    let client_target = target.clone();
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(HANDSHAKE)).unwrap();
        stream.set_write_timeout(Some(HANDSHAKE)).unwrap();
        check_peer(&stream, uid).unwrap();
        let mut identify = [0; 8];
        stream.read_exact(&mut identify).unwrap();
        assert_eq!(identify, wire::IDENTIFY);
        stream
            .write_all(&wire::identity_record(
                wire::Version::V1,
                &target,
                instance,
                Status::default(),
            ))
            .unwrap();
        let mut command = Vec::new();
        (&mut stream)
            .take(u64::try_from(wire::COMMAND_BYTES + wire::MAX_PAYLOAD + 1).unwrap())
            .read_to_end(&mut command)
            .unwrap();
        assert!(command.len() >= wire::COMMAND_BYTES);
        stream.write_all(&[0; wire::RESULT_BYTES]).unwrap();
    });
    assert_eq!(
        send(&client_target, &wire::hex(&instance), &["/one".into()]),
        Err(Error::OutcomeUnknown)
    );
    peer.join().unwrap();
}

#[test]
fn request_bounds_are_checked_before_connect_or_allocation() {
    assert_eq!(validate_target("../target"), Err(Error::InvalidTarget));
    assert_eq!(
        check_target(&"a".repeat(MAX_NAME + 1)),
        Err(Error::InvalidTarget)
    );
    assert_eq!(
        wire::parse_instance("not-an-instance"),
        Err(Error::InvalidInstance)
    );
    assert_eq!(wire::paths(&[]), Err(Error::InvalidPaths));
    assert_eq!(
        wire::paths(&vec![PathBuf::from("/one"); MAX_PATHS + 1]),
        Err(Error::InvalidPaths)
    );
    assert_eq!(wire::paths(&["relative".into()]), Err(Error::InvalidPaths));
    assert_eq!(
        wire::paths(&["/selected/../different".into()]),
        Err(Error::InvalidPaths)
    );
    let parent_path = b"/selected/../different";
    let mut parent = u16::try_from(parent_path.len())
        .unwrap()
        .to_be_bytes()
        .to_vec();
    parent.extend_from_slice(parent_path);
    assert_eq!(wire::decode_paths(&parent, 1), Err(Error::InvalidPaths));
    assert_eq!(
        wire::paths(&[PathBuf::from(OsString::from_vec(b"/nul\0path".to_vec()))]),
        Err(Error::InvalidPaths)
    );
    assert_eq!(
        wire::paths(&[PathBuf::from(format!("/{}", "x".repeat(MAX_PATH_BYTES)))]),
        Err(Error::InvalidPaths)
    );
    let bounded = vec![PathBuf::from(format!("/{}", "x".repeat(MAX_PATH_BYTES - 1))); MAX_PATHS];
    let encoded = wire::paths(&bounded).unwrap();
    assert_eq!(encoded.len(), wire::MAX_PAYLOAD);
    assert_eq!(wire::decode_paths(&encoded, MAX_PATHS).unwrap(), bounded);
}

#[test]
fn status_codec_retains_partial_evidence_and_rejects_ambiguous_refusal_frames() {
    let instance = [31; 16];
    let status = Status {
        sequence: 7,
        ready: false,
        closed: true,
        phase: Phase::Ended,
        stage: Stage::Closed,
        selected: 8,
        started: 7,
        published: 3,
        refused: 1,
        interrupted: 1,
        publication_unknown: 1,
        durability_unknown: 2,
        complete: false,
        failed: true,
        cancel_requested: true,
        pause_requested: None,
        paused: None,
        queued_bytes: Some(12_345),
        total_bytes: Some(50_000),
        cleanup: Cleanup::Failed,
    };
    let response = wire::result_record(wire::Version::V1, instance, Ok((status, true)));
    assert_eq!(
        wire::result(
            wire::Version::V1,
            response.as_slice().try_into().unwrap(),
            instance
        ),
        Ok((status, true))
    );
    let completed = Status {
        complete: true,
        phase: Phase::Finished,
        cleanup: Cleanup::Confirmed,
        ..status
    };
    let response = wire::result_record(wire::Version::V1, instance, Ok((completed, false)));
    assert_eq!(
        wire::result(
            wire::Version::V1,
            response.as_slice().try_into().unwrap(),
            instance
        ),
        Ok((completed, false))
    );
    let refusal = Error::Selection {
        index: 7,
        reason: SelectionFailure::Changed,
    };
    let response = wire::result_record(wire::Version::V1, instance, Err(refusal));
    assert_eq!(
        wire::result(
            wire::Version::V1,
            response.as_slice().try_into().unwrap(),
            instance
        ),
        Err(refusal)
    );
    let mut ambiguous = wire::result_record(wire::Version::V1, instance, Err(Error::Busy));
    ambiguous[28] = 1;
    assert_eq!(
        wire::result(
            wire::Version::V1,
            ambiguous.as_slice().try_into().unwrap(),
            instance
        ),
        Err(Error::OutcomeUnknown)
    );
    let mut ambiguous = wire::result_record(wire::Version::V1, instance, Err(Error::Busy));
    ambiguous[27] = 1;
    assert_eq!(
        wire::result(
            wire::Version::V1,
            ambiguous.as_slice().try_into().unwrap(),
            instance
        ),
        Err(Error::OutcomeUnknown)
    );
}

#[test]
fn versioned_status_keeps_pause_intent_separate_and_legacy_observations_unknown() {
    use wire::Version::{V1, V2};
    let instance = [42; 16];
    for (requested, observed) in [
        (None, None),
        (Some(true), None),
        (Some(true), Some(false)),
        (Some(false), Some(true)),
        (Some(true), Some(true)),
    ] {
        let status = Status {
            sequence: 3,
            selected: 2,
            started: 2,
            published: 1,
            publication_unknown: 1,
            cleanup: Cleanup::Confirmed,
            pause_requested: requested,
            paused: observed,
            ..Status::default()
        };
        let legacy = Status {
            pause_requested: None,
            paused: None,
            ..status
        };
        for (version, expected) in [(V1, legacy), (V2, status)] {
            let response = wire::result_record(version, instance, Ok((status, false)));
            assert_eq!(response.len(), wire::RESULT_BYTES);
            assert_eq!(
                wire::result(version, response.as_slice().try_into().unwrap(), instance),
                Ok((expected, false))
            );
            let identity = wire::identity_record(version, "owner", instance, status);
            assert_eq!(
                wire::identity(
                    version,
                    identity[..wire::IDENTITY_BYTES].try_into().unwrap()
                ),
                Ok((instance, 5, expected))
            );
        }
    }
}

#[test]
fn pacing_commands_require_v2_exact_request_and_empty_payload() {
    use wire::Version::{V1, V2};
    let instance = [43; 16];
    for operation in [wire::Command::Pause(8), wire::Command::Resume(8)] {
        assert_eq!(
            wire::command(V1, operation, instance, 0),
            Err(Error::InvalidRequest)
        );
        let header = wire::command(V2, operation, instance, 0).unwrap();
        let (parsed, actual, length) = wire::header(V2, &header).unwrap();
        assert!(matches!(
            parsed,
            wire::Command::Pause(8) | wire::Command::Resume(8)
        ));
        assert_eq!((actual, length), (instance, 0));
        assert!(wire::header(V1, &header).is_err());
        let mut legacy_opcode = header;
        legacy_opcode[4] = b'1';
        assert!(wire::header(V1, &legacy_opcode).is_err());
        for offset in [31, 33, 37] {
            let mut malformed = header;
            if offset == 31 {
                malformed[24..32].fill(0);
            } else {
                malformed[offset] = 1;
            }
            assert!(wire::header(V2, &malformed).is_err());
        }
        let mut unknown = header;
        unknown[5..7].copy_from_slice(b"XX");
        assert!(wire::header(V2, &unknown).is_err());
    }
    assert_eq!(
        wire::command(V2, wire::Command::Pause(0), instance, 0),
        Err(Error::InvalidRequest)
    );
    assert_eq!(
        wire::command(V2, wire::Command::Resume(0), instance, 0),
        Err(Error::InvalidRequest)
    );
    assert_eq!(
        upload::pause("unopened", "unopened", 0),
        Err(Error::InvalidRequest)
    );
    assert_eq!(
        upload::resume("unopened", "unopened", 0),
        Err(Error::InvalidRequest)
    );
    assert_eq!(
        wire::Version::from_identify(b"FRFS3ID\0"),
        Err(Error::Protocol)
    );
}

#[test]
fn malformed_or_cross_version_pacing_reply_keeps_outcome_unknown() {
    use wire::Version::{V1, V2};
    let instance = [44; 16];
    let status = Status {
        sequence: 1,
        selected: 1,
        pause_requested: Some(true),
        ..Status::default()
    };
    let valid = wire::result_record(V2, instance, Ok((status, true)));
    assert_eq!(
        wire::result(V1, valid.as_slice().try_into().unwrap(), instance),
        Err(Error::OutcomeUnknown)
    );
    for offset in [28 + 6, 28 + 7, 28 + 30] {
        let mut malformed = valid.clone();
        malformed[offset] = 3;
        assert_eq!(
            wire::result(V2, malformed.as_slice().try_into().unwrap(), instance),
            Err(Error::OutcomeUnknown)
        );
    }
    let mut refusal = wire::result_record(V2, instance, Err(Error::Busy));
    refusal[28 + 6] = 2;
    assert_eq!(
        wire::result(V2, refusal.as_slice().try_into().unwrap(), instance),
        Err(Error::OutcomeUnknown)
    );
}

#[test]
fn live_upload_pacing_targets_only_its_retained_request_and_original_lease() {
    let connection = connection();
    let handler = Arc::new(Probe::default());
    let owner = Owner::bind_upload(&connection, handler.clone()).unwrap();
    let info = upload::inspect(&connection).unwrap();
    for _ in 0..2 {
        upload::send(&connection, &info.instance, &["/explicit".into()]).unwrap();
    }
    let paused = upload::pause(&connection, &info.instance, 1).unwrap();
    assert!(paused.changed);
    assert_eq!(paused.info.status.pause_requested, Some(true));
    assert_eq!(
        paused.info.status.paused, None,
        "recording handler proves delivery, not sender effect"
    );
    assert!(
        !upload::pause(&connection, &info.instance, 1)
            .unwrap()
            .changed
    );
    assert_eq!(
        upload::status(&connection, &info.instance, Some(2))
            .unwrap()
            .status
            .pause_requested,
        Some(false)
    );
    assert_eq!(
        upload::resume(&connection, &"f".repeat(32), 1),
        Err(Error::WrongInstance)
    );
    assert_eq!(
        upload::resume(&connection, &info.instance, 3),
        Err(Error::UnknownRequest)
    );
    assert!(
        upload::resume(&connection, &info.instance, 1)
            .unwrap()
            .changed
    );
    upload::pause(&connection, &info.instance, 1).unwrap();
    upload::cancel(&connection, &info.instance, 1).unwrap();
    let terminal = upload::resume(&connection, &info.instance, 1).unwrap();
    assert!(!terminal.changed);
    assert!(terminal.info.status.cancel_requested);
    assert_eq!(terminal.info.status.pause_requested, Some(true));
    assert_eq!(handler.records.lock().unwrap().paths.len(), 2);
    stop(&owner);
}

/// Real old-client framing against the current upload owner, including kernel
/// credentials and the original single budget; no fallback or second selection.
fn legacy_upload(
    target: &str,
    instance: [u8; 16],
    header: [u8; wire::COMMAND_BYTES],
    payload: &[u8],
) -> Result<(Status, bool), Error> {
    let until = Instant::now().checked_add(HANDSHAKE).unwrap();
    let uid = rustix::process::geteuid().as_raw();
    let mut stream = connect(Scope::Upload, uid, target)?;
    check_peer(&stream, uid)?;
    write(&mut stream, &wire::IDENTIFY, until)?;
    let mut identity = [0; wire::IDENTITY_BYTES];
    read(&mut stream, &mut identity, until)?;
    let (actual, length, _) = wire::identity(wire::Version::V1, &identity)?;
    assert_eq!(actual, instance);
    let mut name = [0; MAX_NAME];
    read(&mut stream, &mut name[..length], until)?;
    assert_eq!(&name[..length], target.as_bytes());
    write(&mut stream, &header, until)?;
    write(&mut stream, payload, until)?;
    stream.shutdown(Shutdown::Write).unwrap();
    let mut response = [0; wire::RESULT_BYTES];
    read(&mut stream, &mut response, until)?;
    wire::result(wire::Version::V1, &response, instance)
}

#[test]
fn current_owner_serves_old_upload_clients_and_refuses_old_pacing_opcodes() {
    use wire::Version::{V1, V2};
    let connection = connection();
    let handler = Arc::new(Probe::default());
    let owner = Owner::bind_upload(&connection, handler.clone()).unwrap();
    let info = owner.info().unwrap();
    let instance = wire::parse_instance(&info.instance).unwrap();
    let payload = wire::paths(&["/old-client-selection".into()]).unwrap();
    let send = wire::command(V1, wire::Command::Send(1), instance, payload.len()).unwrap();
    let (status, _) = legacy_upload(&connection, instance, send, &payload).unwrap();
    assert_eq!(status.sequence, 1);
    assert_eq!(
        status.pause_requested, None,
        "v1 cannot report a new observation as false"
    );
    upload::pause(&connection, &info.instance, 1).unwrap();
    let status = wire::command(V1, wire::Command::Status(Some(1)), instance, 0).unwrap();
    assert_eq!(
        legacy_upload(&connection, instance, status, &[])
            .unwrap()
            .0
            .paused,
        None
    );
    let mut unsupported = wire::command(V2, wire::Command::Resume(1), instance, 0).unwrap();
    unsupported[4] = b'1';
    assert_eq!(
        legacy_upload(&connection, instance, unsupported, &[]),
        Err(Error::Protocol)
    );
    assert_eq!(handler.records.lock().unwrap().pause_requested, vec![true]);
    let cancel = wire::command(V1, wire::Command::Cancel(1), instance, 0).unwrap();
    assert!(legacy_upload(&connection, instance, cancel, &[]).unwrap().1);
    stop(&owner);
}

#[test]
fn new_upload_client_refuses_legacy_owner_before_command_without_fallback() {
    let connection = connection();
    let uid = rustix::process::geteuid().as_raw();
    let address = SocketAddr::from_abstract_name(name(Scope::Upload, uid, &connection)).unwrap();
    let listener = UnixListener::bind_addr(&address).unwrap();
    let instance = [74; 16];
    let client_target = connection.clone();
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(HANDSHAKE)).unwrap();
        stream.set_write_timeout(Some(HANDSHAKE)).unwrap();
        let mut identify = [0; 8];
        stream.read_exact(&mut identify).unwrap();
        assert_eq!(identify, wire::Version::V2.identify());
        // A legacy owner cannot authenticate a V2 status contract. Returning
        // its old identity must fail before any command write, with no retry.
        stream
            .write_all(&wire::identity_record(
                wire::Version::V1,
                &connection,
                instance,
                Status::default(),
            ))
            .unwrap();
        let mut command = [0; wire::COMMAND_BYTES];
        match stream.read(&mut command) {
            Ok(0) => {}
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
            result => panic!("legacy owner received command bytes: {result:?}"),
        }
    });
    assert_eq!(
        upload::pause(&client_target, &wire::hex(&instance), 9),
        Err(Error::Protocol),
        "a post-command failure would instead be OutcomeUnknown"
    );
    peer.join().unwrap();
}

#[test]
fn v2_pacing_post_command_lost_malformed_or_legacy_reply_is_unknown_without_replay() {
    for mode in 0..3 {
        let connection = connection();
        let uid = rustix::process::geteuid().as_raw();
        let address =
            SocketAddr::from_abstract_name(name(Scope::Upload, uid, &connection)).unwrap();
        let listener = UnixListener::bind_addr(&address).unwrap();
        let instance = [75; 16];
        let client_target = connection.clone();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(HANDSHAKE)).unwrap();
            stream.set_write_timeout(Some(HANDSHAKE)).unwrap();
            let mut identify = [0; 8];
            stream.read_exact(&mut identify).unwrap();
            assert_eq!(identify, wire::Version::V2.identify());
            stream
                .write_all(&wire::identity_record(
                    wire::Version::V2,
                    &connection,
                    instance,
                    Status::default(),
                ))
                .unwrap();
            let mut command = [0; wire::COMMAND_BYTES];
            stream.read_exact(&mut command).unwrap();
            let (operation, original, length) = wire::header(wire::Version::V2, &command).unwrap();
            assert!(matches!(operation, wire::Command::Pause(9)));
            assert_eq!((original, length), (instance, 0));
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
            if mode == 0 {
                return;
            }
            let status = Status {
                sequence: 9,
                pause_requested: Some(true),
                ..Status::default()
            };
            let version = if mode == 1 {
                wire::Version::V1
            } else {
                wire::Version::V2
            };
            let mut response = wire::result_record(version, instance, Ok((status, true)));
            if mode == 2 {
                response[28 + 7] = 3;
            }
            stream.write_all(&response).unwrap();
        });
        assert_eq!(
            upload::pause(&client_target, &wire::hex(&instance), 9),
            Err(Error::OutcomeUnknown)
        );
        peer.join().unwrap();
    }
}
