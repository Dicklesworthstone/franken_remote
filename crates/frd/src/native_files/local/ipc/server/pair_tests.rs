//! Actual Peer framing and kernel credentials over real Unix socket pairs.
//! The handler records dispatch only; no source, remote authority or file
//! publication is simulated, and abstract namespace admission is tested elsewhere.
use super::super::{read, write};
use super::*;
use std::sync::Mutex;

const INSTANCE: [u8; 16] = [71; 16];
const TARGET: &str = "0123456789abcdef0123456789abcdef";

struct Recording {
    status: Mutex<Status>,
    pacing: Mutex<Vec<(u64, bool)>>,
}
impl Recording {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(Status {
                sequence: 7,
                selected: 1,
                pause_requested: Some(false),
                paused: Some(false),
                ..Status::default()
            }),
            pacing: Mutex::new(Vec::new()),
        })
    }
}
impl Handler for Recording {
    fn status(&self, sequence: Option<u64>) -> Result<Status, Error> {
        let status = *self.status.lock().unwrap();
        if sequence.is_some_and(|sequence| sequence != status.sequence) {
            return Err(Error::UnknownRequest);
        }
        Ok(status)
    }
    fn select(&self, _: &[PathBuf], _: Instant, _: &AtomicBool) -> Result<Status, Error> {
        // Pacing/status must never invoke a selector.
        panic!("unexpected selection");
    }
    fn cancel(&self, _: u64) -> Result<(Status, bool), Error> {
        panic!("pacing must not cancel");
    }
    fn set_paused(&self, sequence: u64, paused: bool) -> Result<(Status, bool), Error> {
        let mut status = self.status.lock().unwrap();
        if sequence != status.sequence {
            return Err(Error::UnknownRequest);
        }
        self.pacing.lock().unwrap().push((sequence, paused));
        let changed = status.pause_requested != Some(paused);
        status.pause_requested = Some(paused);
        Ok((*status, changed))
    }
    fn close(&self) {}
}

struct Exchange {
    identified: Status,
    response: Result<(Status, bool), Error>,
}
fn exchange(
    version: wire::Version,
    handler: &Arc<Recording>,
    request: [u8; wire::COMMAND_BYTES],
) -> Exchange {
    let (mut client, server) = UnixStream::pair().unwrap();
    client.set_nonblocking(true).unwrap();
    let uid = rustix::process::geteuid().as_raw();
    check_peer(&client, uid).unwrap();
    check_peer(&server, uid).unwrap();
    let shared = Shared {
        handler: handler.clone(),
        stopped: AtomicBool::new(false),
        selecting: AtomicBool::new(false),
    };
    let server = thread::spawn(move || {
        let mut peer = Peer::new(server).unwrap();
        let (submit, _jobs) = mpsc::sync_channel(1);
        loop {
            match peer.advance(&shared, &submit, TARGET, INSTANCE) {
                Ok(true) => return,
                Ok(false) => thread::sleep(TICK),
                Err(error) => panic!("peer failed: {error:?}"),
            }
        }
    });
    let until = Instant::now().checked_add(HANDSHAKE).unwrap();
    write(&mut client, &version.identify(), until).unwrap();
    let mut identity = [0; wire::IDENTITY_BYTES];
    read(&mut client, &mut identity, until).unwrap();
    let (instance, length, identified) = wire::identity(version, &identity).unwrap();
    assert_eq!(instance, INSTANCE);
    let mut name = vec![0; length];
    read(&mut client, &mut name, until).unwrap();
    assert_eq!(name, TARGET.as_bytes());
    write(&mut client, &request, until).unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();
    let mut reply = [0; wire::RESULT_BYTES];
    read(&mut client, &mut reply, until).unwrap();
    let response = wire::result(version, &reply, instance);
    server.join().unwrap();
    Exchange {
        identified,
        response,
    }
}

#[test]
fn actual_peer_delivers_exact_pacing_request_without_new_selection_or_fabricated_observation() {
    use wire::Version::V2;
    let handler = Recording::new();
    let pause = wire::command(V2, wire::Command::Pause(7), INSTANCE, 0).unwrap();
    let reply = exchange(V2, &handler, pause);
    let (status, changed) = reply.response.unwrap();
    assert!(changed);
    assert_eq!(status.sequence, 7);
    assert_eq!(status.pause_requested, Some(true));
    assert_eq!(
        status.paused,
        Some(false),
        "dispatch cannot fabricate sender acknowledgement"
    );
    assert!(!exchange(V2, &handler, pause).response.unwrap().1);
    handler.status.lock().unwrap().paused = Some(true);
    let resume = wire::command(V2, wire::Command::Resume(7), INSTANCE, 0).unwrap();
    let (status, changed) = exchange(V2, &handler, resume).response.unwrap();
    assert!(changed);
    assert_eq!(status.pause_requested, Some(false));
    assert_eq!(
        status.paused,
        Some(true),
        "recorded sample stays independent from latest intent"
    );
    assert_eq!(
        *handler.pacing.lock().unwrap(),
        vec![(7, true), (7, true), (7, false)]
    );
}

#[test]
fn actual_peer_keeps_v1_status_compatible_and_v2_samples_explicit() {
    use wire::Version::{V1, V2};
    let handler = Recording::new();
    handler.status.lock().unwrap().pause_requested = Some(true);
    for version in [V1, V2] {
        let request = wire::command(version, wire::Command::Status(Some(7)), INSTANCE, 0).unwrap();
        let reply = exchange(version, &handler, request);
        let status = reply.response.unwrap().0;
        assert_eq!(status.sequence, 7);
        let requested = if version == V2 { Some(true) } else { None };
        let observed = if version == V2 { Some(false) } else { None };
        assert_eq!(reply.identified.pause_requested, requested);
        assert_eq!(reply.identified.paused, observed);
        assert_eq!(status.pause_requested, requested);
        assert_eq!(status.paused, observed);
    }
    assert!(handler.pacing.lock().unwrap().is_empty());
}

#[test]
fn actual_peer_refuses_mixed_version_unknown_and_stale_requests_without_dispatch() {
    use wire::Version::{V1, V2};
    let handler = Recording::new();
    let v2 = wire::command(V2, wire::Command::Pause(7), INSTANCE, 0).unwrap();
    assert_eq!(exchange(V1, &handler, v2).response, Err(Error::Protocol));
    let mut legacy_unknown = v2;
    legacy_unknown[4] = b'1';
    assert_eq!(
        exchange(V1, &handler, legacy_unknown).response,
        Err(Error::Protocol)
    );
    let mut unknown = v2;
    unknown[5..7].copy_from_slice(b"XX");
    assert_eq!(
        exchange(V2, &handler, unknown).response,
        Err(Error::Protocol)
    );
    let v1 = wire::command(V1, wire::Command::Status(Some(7)), INSTANCE, 0).unwrap();
    assert_eq!(exchange(V2, &handler, v1).response, Err(Error::Protocol));
    let stale = wire::command(V2, wire::Command::Pause(7), [72; 16], 0).unwrap();
    assert_eq!(
        exchange(V2, &handler, stale).response,
        Err(Error::WrongInstance)
    );
    let absent = wire::command(V2, wire::Command::Pause(8), INSTANCE, 0).unwrap();
    assert_eq!(
        exchange(V2, &handler, absent).response,
        Err(Error::UnknownRequest)
    );
    assert!(handler.pacing.lock().unwrap().is_empty());
    assert_eq!(handler.status.lock().unwrap().pause_requested, Some(false));
}
