#![forbid(unsafe_code)]
//! Internal read-only role. Configuration travels only over inherited stdin;
//! stdout is fixed-size binary IPC. No command-line grants, UI, capture or input.
#[cfg(any(
    all(target_os = "linux", feature = "linux-session-monitor"),
    all(target_os = "macos", feature = "macos-session-monitor")
))]
fn main() -> std::process::ExitCode {
    if std::env::args_os().len() != 1 {
        return std::process::ExitCode::from(2);
    }
    if run().is_ok() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(1)
    }
}
#[cfg(not(any(
    all(target_os = "linux", feature = "linux-session-monitor"),
    all(target_os = "macos", feature = "macos-session-monitor")
)))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(2)
}

#[cfg(any(
    all(target_os = "linux", feature = "linux-session-monitor"),
    all(target_os = "macos", feature = "macos-session-monitor")
))]
#[path = "session_monitor/pipe.rs"]
mod pipe;

#[cfg(all(target_os = "linux", feature = "linux-session-monitor"))]
fn run() -> Result<(), ()> {
    use fr_native::logind::{self, StopReason};
    use frd::session_monitor::protocol::{self, Platform, Reply, Selection, State};
    use std::io::Read;
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let mut bytes = [0; protocol::SELECTION_BYTES];
    input.read_exact(&mut bytes).map_err(|_| ())?;
    let (selected, epoch) = Selection::decode(&bytes).map_err(|_| ())?;
    if selected.platform != Platform::Linux {
        return Err(());
    }
    let watch = logind::Watch::start(logind::Selection {
        session: selected.session,
        uid: selected.uid,
        seat: selected.seat,
        display: selected.display,
    })
    .map_err(|_| ())?;
    let control = watch.control();
    // The parent independently enforces evidence/IPC deadlines and reaps this
    // process. Buffered queries cannot renew stopped native evidence.
    pipe::serve(&mut input, &mut output, epoch, || match control.status() {
        logind::Status::Opening => Reply {
            state: State::Opening,
            until_ns: 0,
        },
        logind::Status::Active => match control.evidence_deadline_ns() {
            Some(until_ns) => Reply {
                state: State::Active,
                until_ns,
            },
            None => Reply {
                state: State::EvidenceExpired,
                until_ns: 0,
            },
        },
        logind::Status::Stopped(reason) => Reply {
            until_ns: 0,
            state: match reason {
                StopReason::Locked => State::Locked,
                StopReason::Inactive => State::Inactive,
                StopReason::Suspending => State::Suspending,
                StopReason::SessionUnavailable => State::SessionUnavailable,
                StopReason::IdentityChanged => State::IdentityChanged,
                StopReason::BusUnavailable | StopReason::UntrustedService => {
                    State::ServiceUnavailable
                }
                StopReason::UnsupportedSession => State::UnsupportedSession,
                StopReason::EvidenceExpired => State::EvidenceExpired,
                _ => State::Failed,
            },
        },
    })
}

#[cfg(all(target_os = "macos", feature = "macos-session-monitor"))]
fn run() -> Result<(), ()> {
    use fr_native::macos_session::{Control, Monitor};
    use frd::session_monitor::protocol::{self, Platform, Selection, State};
    use std::{io::Read, thread, time::Duration};

    let mut input = std::io::stdin();
    let mut output = std::io::stdout();
    let mut bytes = [0; protocol::SELECTION_BYTES];
    input.read_exact(&mut bytes).map_err(|_| ())?;
    let (selected, epoch) = Selection::decode(&bytes).map_err(|_| ())?;
    if selected.platform != Platform::Macos {
        return Err(());
    }
    let control = Control::opening(selected.uid);
    let responder = control.clone();
    let worker = thread::Builder::new()
        .name("fr-session-evidence-ipc".into())
        .spawn(move || {
            // AppKit can take longer to open than the parent's 250 ms IPC
            // transaction. Reply Opening while it does; this thread can only
            // read/expire evidence, never stamp a fresh native observation.
            let result = pipe::serve(&mut input, &mut output, epoch, || responder.status());
            responder.fail();
            // EOF, invalid IPC or a delivered terminal report ends this read-only
            // process even if its main thread is stuck inside a framework. The
            // parent retains its exact child and still performs kill/reap; this
            // is no evidence of input release or native media cleanup.
            std::process::exit(i32::from(result.is_err()));
        })
        .map_err(|_| ())?;

    let mut monitor = Monitor::open(control.clone()).ok();
    while !worker.is_finished() {
        if matches!(control.status().state, State::Opening | State::Active) {
            if let Some(monitor) = monitor.as_mut() {
                monitor.turn(Duration::from_millis(20));
            } else {
                control.fail();
            }
        } else {
            // Retain the first negative result until the waiting query writes
            // it. No observer or late native callback can resume this epoch.
            thread::sleep(Duration::from_millis(10));
        }
    }
    control.fail();
    worker.join().map_err(|_| ())
}
