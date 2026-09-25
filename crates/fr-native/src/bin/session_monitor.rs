#![forbid(unsafe_code)]
//! Internal read-only role. Configuration travels only over inherited stdin;
//! stdout is fixed-size binary IPC. No command-line grants, UI, capture or input.
#[cfg(target_os = "linux")]
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
#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(2)
}

#[cfg(target_os = "linux")]
fn run() -> Result<(), ()> {
    use fr_native::logind::{self, StopReason};
    use frd::session_monitor::protocol::{self, Reply, Selection, State};
    use std::io::{Read, Write};
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let mut bytes = [0; protocol::SELECTION_BYTES];
    input.read_exact(&mut bytes).map_err(|_| ())?;
    let (selected, epoch) = Selection::decode(&bytes).map_err(|_| ())?;
    let watch = logind::Watch::start(logind::Selection {
        session: selected.session,
        uid: selected.uid,
        seat: selected.seat,
        display: selected.display,
    })
    .map_err(|_| ())?;
    let control = watch.control();
    let mut sequence = 1_u64;
    loop {
        let mut bytes = [0; protocol::QUERY_BYTES];
        // The parent independently enforces evidence/IPC deadlines and reaps this
        // process. Its pipe closing is terminal; buffered old queries cannot renew
        // a stopped logind owner. The native watchdog uses CLOCK_BOOTTIME itself.
        if input.read_exact(&mut bytes).is_err() {
            return Ok(());
        }
        protocol::check_query(&bytes, epoch, sequence).map_err(|_| ())?;
        let reply = match control.status() {
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
        };
        output
            .write_all(&reply.encode(epoch, sequence).map_err(|_| ())?)
            .map_err(|_| ())?;
        output.flush().map_err(|_| ())?;
        if !matches!(reply.state, State::Opening | State::Active) {
            return Ok(());
        }
        sequence = sequence.checked_add(1).ok_or(())?;
    }
}
