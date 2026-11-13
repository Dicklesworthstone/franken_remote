//! Windows: the logon session this process runs in, read through the
//! Terminal Services API (`fr_winsys::session`) on the monitor thread itself.
//! There is no helper process: the reads are local calls that answer from
//! the session manager's own state. The state they map to is decided by
//! `state`, which builds and is tested on every platform.
use super::protocol::State;

/// One reading of a Windows session, reduced to what decides its state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reading {
    /// The session is the one attached to the physical console.
    pub on_console: bool,
    /// Connected and in use (`WTSActive`).
    pub active: bool,
    /// `Some(true)` locked, `Some(false)` unlocked, `None` Windows did not say.
    pub locked: Option<bool>,
    /// The same logon (user, domain, logon time) as when sharing began.
    pub same_logon: bool,
}

/// The state one reading (`None`: the session could not be read) stands for.
/// Only an unlocked, active console session of the original logon is Active.
pub(crate) const fn state(reading: Option<Reading>) -> State {
    match reading {
        None => State::SessionUnavailable,
        Some(r) if !r.same_logon => State::IdentityChanged,
        Some(r) if !r.active || !r.on_console => State::Inactive,
        Some(Reading {
            locked: Some(true), ..
        }) => State::Locked,
        Some(Reading { locked: None, .. }) => State::UnsupportedSession,
        Some(_) => State::Active,
    }
}

#[cfg(windows)]
pub(super) use native::{run, selected};

#[cfg(windows)]
mod native {
    use super::super::{
        Configuration, Error, OCCUPIED, Shared, Status, now_ns,
        protocol::{Platform, Reply, State},
    };
    use super::{Reading, state};
    use fr_winsys::session::{self, Lock};
    use std::{sync::atomic::Ordering, time::Duration};

    /// From one read to the next.
    const PERIOD_NS: u64 = 50_000_000;
    /// How long one Active read stands, from before it was taken (as the
    /// Linux logind helper's 500 ms).
    const READ_NS: u64 = 500_000_000;
    /// Between checks for a stop while waiting for the next read.
    const SLICE: Duration = Duration::from_millis(5);

    /// The session a Windows selection names, which must be this process's
    /// own. A Windows monitor has no helper image.
    pub(in super::super) fn selected(configuration: &Configuration) -> Result<u32, Error> {
        let selection = &configuration.selection;
        if selection.platform != Platform::Windows || selection.validate().is_err() {
            return Err(Error::Selection);
        }
        if !configuration.image.as_os_str().is_empty() {
            return Err(Error::Image);
        }
        let session = selection
            .session
            .parse::<u32>()
            .map_err(|_| Error::Selection)?;
        if session::current().map_err(|_| Error::Selection)? != session {
            return Err(Error::Selection);
        }
        Ok(session)
    }

    pub(in super::super) fn run(session: u32, shared: &Shared) {
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| serve(session, shared)));
        shared.stop(
            result
                .unwrap_or(Err(Error::Protocol))
                .err()
                .unwrap_or(Error::Stopped),
        );
        OCCUPIED.store(false, Ordering::Release);
    }

    fn serve(session: u32, shared: &Shared) -> Result<(), Error> {
        // The logon sharing began in; every later read must still be it.
        let first = session::read(session)
            .map_err(|_| Error::Native(State::SessionUnavailable))?
            .logon;
        if !first.present() {
            return Err(Error::Native(State::UnsupportedSession));
        }
        loop {
            let sent = now_ns()?;
            let reading = session::read(session).ok().map(|r| Reading {
                on_console: session::console() == Some(session),
                active: r.active,
                locked: match r.lock {
                    Lock::Locked => Some(true),
                    Lock::Unlocked => Some(false),
                    Lock::Unknown => None,
                },
                same_logon: r.logon == first,
            });
            let state = state(reading);
            let until_ns = if state == State::Active {
                sent.checked_add(READ_NS).ok_or(Error::Clock)?
            } else {
                0
            };
            // Anything but Active (or Opening) stops the monitor here.
            shared.accept(Reply { state, until_ns }, sent)?;
            let next = sent.checked_add(PERIOD_NS).ok_or(Error::Clock)?;
            while now_ns()? < next {
                if let Status::Stopped(error) = shared.status() {
                    return Err(error);
                }
                std::thread::sleep(SLICE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTIVE: Reading = Reading {
        on_console: true,
        active: true,
        locked: Some(false),
        same_logon: true,
    };

    #[test]
    fn only_an_unlocked_active_console_logon_is_active() {
        assert_eq!(state(Some(ACTIVE)), State::Active);
        let cases = [
            (None, State::SessionUnavailable),
            (
                Some(Reading {
                    locked: Some(true),
                    ..ACTIVE
                }),
                State::Locked,
            ),
            (
                Some(Reading {
                    locked: None,
                    ..ACTIVE
                }),
                State::UnsupportedSession,
            ),
            // Switched away from (fast user switching) or disconnected.
            (
                Some(Reading {
                    active: false,
                    ..ACTIVE
                }),
                State::Inactive,
            ),
            // Taken over remotely: the console shows another session.
            (
                Some(Reading {
                    on_console: false,
                    ..ACTIVE
                }),
                State::Inactive,
            ),
            // Logged off and on again in the same session: a new logon,
            // whatever else it reads as.
            (
                Some(Reading {
                    same_logon: false,
                    ..ACTIVE
                }),
                State::IdentityChanged,
            ),
            (
                Some(Reading {
                    same_logon: false,
                    locked: Some(true),
                    active: false,
                    on_console: false,
                }),
                State::IdentityChanged,
            ),
        ];
        for (reading, expected) in cases {
            assert_eq!(state(reading), expected, "{reading:?}");
        }
    }
}
