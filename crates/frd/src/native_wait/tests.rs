//! Real private descriptors: readiness does not consume a reply or a local stop.
use super::*;
use std::{
    io::{Read, Write},
    os::unix::net::UnixDatagram,
    sync::{Arc, mpsc},
    thread,
    time::Instant,
};

const WAIT: Duration = Duration::from_secs(10);
const JOIN: Duration = Duration::from_secs(2);

#[test]
fn stopping_a_parked_wait_wakes_it_and_remains_terminal_for_later_waits() {
    let wake = Arc::new(Wake::new().unwrap());
    let (command, _peer) = UnixStream::pair().unwrap();
    let original = wake.clone();
    let (enter, entered) = mpsc::sync_channel(0);
    let (finish, finished) = mpsc::sync_channel(0);
    let worker = thread::spawn(move || {
        enter.send(()).unwrap();
        original
            .wait(PollFd::new(&command, PollFlags::IN), None, WAIT)
            .unwrap();
        finish.send(()).unwrap();
        // Nothing drains the terminal wake, including a later wait on another
        // interest. Repeated stops allocate no records or wake-byte backlog.
        for _ in 0..4 {
            original
                .wait(PollFd::new(&command, PollFlags::IN), None, WAIT)
                .unwrap();
        }
        finish.send(()).unwrap();
    });
    entered.recv_timeout(JOIN).unwrap();
    assert!(finished.recv_timeout(Duration::from_millis(25)).is_err());
    for _ in 0..32 {
        wake.signal();
    }
    finished.recv_timeout(JOIN).unwrap();
    finished.recv_timeout(JOIN).unwrap();
    worker.join().unwrap();
}

#[test]
fn reply_readiness_wakes_without_consuming_or_rewriting_the_original_bytes() {
    let wake = Wake::new().unwrap();
    let (mut command, mut peer) = UnixStream::pair().unwrap();
    command.set_nonblocking(true).unwrap();
    peer.write_all(b"original").unwrap();
    wake.wait(PollFd::new(&command, PollFlags::IN), None, WAIT)
        .unwrap();
    let mut bytes = [0; 8];
    command.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"original");
}

#[test]
fn terminal_datagram_readiness_does_not_consume_the_owners_signal() {
    let wake = Wake::new().unwrap();
    let (command, _peer) = UnixStream::pair().unwrap();
    let (signals, local) = UnixDatagram::pair().unwrap();
    signals.set_nonblocking(true).unwrap();
    local.send(b"stop").unwrap();
    wake.wait(
        PollFd::new(&command, PollFlags::IN),
        Some(PollFd::new(&signals, PollFlags::IN)),
        WAIT,
    )
    .unwrap();
    let mut bytes = [0; 4];
    assert_eq!(signals.recv(&mut bytes).unwrap(), 4);
    assert_eq!(&bytes, b"stop");
}

#[test]
fn command_hangup_and_write_readiness_are_observed_without_retry_sleeps() {
    let wake = Wake::new().unwrap();
    let (mut command, peer) = UnixStream::pair().unwrap();
    command.set_nonblocking(true).unwrap();
    wake.wait(PollFd::new(&command, PollFlags::OUT), None, WAIT)
        .unwrap();
    drop(peer);
    wake.wait(PollFd::new(&command, PollFlags::IN), None, WAIT)
        .unwrap();
    assert_eq!(command.read(&mut [0]).unwrap(), 0);
}

#[test]
fn idle_wait_spends_only_the_original_remaining_deadline() {
    let wake = Wake::new().unwrap();
    let (command, _peer) = UnixStream::pair().unwrap();
    let until = Instant::now() + Duration::from_millis(25);
    while let Some(remaining) = until.checked_duration_since(Instant::now()) {
        wake.wait(PollFd::new(&command, PollFlags::IN), None, remaining)
            .unwrap();
    }
    assert!(Instant::now() >= until);
    wake.wait(PollFd::new(&command, PollFlags::IN), None, Duration::ZERO)
        .unwrap();
}
