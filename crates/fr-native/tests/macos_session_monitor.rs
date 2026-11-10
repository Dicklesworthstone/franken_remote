//! The real macOS `fr-session-monitor` over its private pipe protocol,
//! launched the way frd's `session_monitor::process` launches it: cleared
//! environment, the selection and then one query per turn on stdin, fixed-size
//! replies on stdout.
//!
//! It needs this user's logged-in, unlocked graphical session with the display
//! awake, which no headless runner promises, so it is `#[ignore]`d; run it at
//! the Mac with `--ignored`. Nothing is captured, shown or posted.
#![cfg(all(target_os = "macos", feature = "macos-session-monitor"))]
use frd::session_monitor::protocol::{self, Reply, Selection, State};
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

/// On macOS 27.0.1 this helper refused every session before `AppKit`'s no-op
/// activation-policy answer (`UnsupportedSession`) and then the missing
/// console set (`ServiceUnavailable`) were accepted, so frd could not start.
#[test]
#[ignore = "needs this user's logged-in, unlocked graphical session (nothing is captured or posted)"]
fn the_console_session_gives_renewing_active_evidence() {
    let epoch = 0x4652_534d_7465_7374_u128;
    let uid = rustix::process::geteuid().as_raw();
    let mut monitor = Command::new(env!("CARGO_BIN_EXE_fr-session-monitor"))
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = monitor.stdin.take().unwrap();
    let mut output = monitor.stdout.take().unwrap();
    input
        .write_all(&Selection::macos(uid).encode(epoch).unwrap())
        .unwrap();
    let mut states = Vec::new();
    let mut deadlines = Vec::new();
    for sequence in 1..=20 {
        input
            .write_all(&protocol::query(epoch, sequence).unwrap())
            .unwrap();
        let mut bytes = [0; protocol::REPLY_BYTES];
        output.read_exact(&mut bytes).unwrap();
        let reply = Reply::decode(&bytes, epoch, sequence).unwrap();
        states.push(reply.state);
        if reply.state == State::Active {
            deadlines.push(reply.until_ns);
        }
        if !matches!(reply.state, State::Opening | State::Active) || deadlines.len() == 3 {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    drop(input);
    let exit = monitor.wait().unwrap();
    assert!(
        states
            .iter()
            .all(|state| matches!(state, State::Opening | State::Active)),
        "{states:?}"
    );
    assert_eq!(deadlines.len(), 3, "{states:?}");
    assert!(
        deadlines.windows(2).all(|pair| pair[1] > pair[0]),
        "{deadlines:?}"
    );
    assert!(exit.success(), "{exit}");
}
