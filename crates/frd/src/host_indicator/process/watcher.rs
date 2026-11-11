//! Linux observation-watchdog wait on the same suspend-inclusive clock as its
//! evidence. No timer restarts at wake, and no relative sleep hides time asleep.
use rustix::{
    event::{EventfdFlags, eventfd},
    fd::OwnedFd,
    io::{self, Errno, write},
};
#[cfg(any(host_stack, test))]
use rustix::{
    event::{PollFd, PollFlags, poll},
    io::read,
    time::{
        Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, timerfd_create,
        timerfd_settime,
    },
};
#[cfg(any(host_stack, test))]
use std::time::Duration;

/// A coalescing notification counter, not a queue of events or evidence. One
/// watcher consumes it; shared state retains the original cause and deadline.
pub(super) struct Wake(OwnedFd);
impl Wake {
    pub(super) fn new() -> io::Result<Self> {
        eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).map(Self)
    }
    pub(super) fn signal(&self) -> bool {
        for _ in 0..4 {
            match write(&self.0, &1_u64.to_ne_bytes()) {
                Ok(8) | Err(Errno::AGAIN) => return true,
                Err(Errno::INTR) => {}
                _ => return false,
            }
        }
        false
    }
}

#[cfg(any(host_stack, test))]
pub(super) struct Deadline(OwnedFd);
#[cfg(any(host_stack, test))]
impl Deadline {
    pub(super) fn new() -> io::Result<Self> {
        timerfd_create(
            TimerfdClockId::Boottime,
            TimerfdFlags::CLOEXEC | TimerfdFlags::NONBLOCK,
        )
        .map(Self)
    }
    fn arm(&self, until: u64) -> io::Result<()> {
        // Zero disarms a timerfd; the original owner never admits that bound.
        if until == 0 {
            return Err(Errno::INVAL);
        }
        let value = Timespec::try_from(Duration::from_nanos(until)).map_err(|_| Errno::INVAL)?;
        timerfd_settime(
            &self.0,
            TimerfdTimerFlags::ABSTIME,
            &Itimerspec {
                it_interval: Timespec::default(),
                it_value: value,
            },
        )?;
        Ok(())
    }
    pub(super) fn wait(&self, wake: &Wake, until: u64) -> io::Result<()> {
        self.arm(until)?;
        let mut fds = [
            PollFd::new(&self.0, PollFlags::IN),
            PollFd::new(&wake.0, PollFlags::IN),
        ];
        match poll(&mut fds, None) {
            Ok(_) => {}
            Err(Errno::INTR) => return Ok(()),
            Err(error) => return Err(error),
        }
        if fds.iter().any(|fd| {
            fd.revents()
                .intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL)
        }) {
            return Err(Errno::IO);
        }
        if fds[1].revents().contains(PollFlags::IN) {
            match read(&wake.0, &mut [0_u8; 8]) {
                Ok(8) | Err(Errno::INTR) => {}
                Ok(_) => return Err(Errno::IO),
                Err(error) => return Err(error),
            }
        }
        // Re-arming clears the timer's old readiness. Every caller rechecks
        // the original shared evidence before arming the next absolute bound.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readable(fd: &OwnedFd) -> bool {
        let mut fds = [PollFd::new(fd, PollFlags::IN)];
        poll(&mut fds, Some(&Timespec::default())).unwrap();
        fds[0].revents().contains(PollFlags::IN)
    }

    #[test]
    fn absolute_deadline_readiness_and_rearming_keep_the_original_clock() {
        let deadline = Deadline::new().unwrap();
        assert_eq!(deadline.arm(0), Err(Errno::INVAL));
        let now = super::super::now_ns().unwrap();
        deadline.arm(now.saturating_sub(1).max(1)).unwrap();
        let mut fds = [PollFd::new(&deadline.0, PollFlags::IN)];
        // Bounded kernel wait: a past absolute deadline is immediately due,
        // without the original state owner or a polling thread advancing it.
        assert_eq!(
            poll(
                &mut fds,
                Some(&Timespec::try_from(Duration::from_secs(1)).unwrap())
            )
            .unwrap(),
            1
        );
        assert!(fds[0].revents().contains(PollFlags::IN));
        // Old readiness cannot leak into a genuinely later original bound.
        deadline
            .arm(super::super::now_ns().unwrap() + 3_000_000_000)
            .unwrap();
        assert!(!readable(&deadline.0));
    }

    #[test]
    fn notifications_survive_the_check_wait_race_and_coalesce() {
        let wake = Wake::new().unwrap();
        assert!(!readable(&wake.0));
        for _ in 0..3 {
            assert!(wake.signal());
        }
        assert!(readable(&wake.0));
        let deadline = Deadline::new().unwrap();
        let until = super::super::now_ns().unwrap() + 3_000_000_000;
        deadline.wait(&wake, until).unwrap();
        assert!(super::super::now_ns().unwrap() < until);
        // One read consumes the bounded counter, including coalesced events.
        assert!(!readable(&wake.0));
        assert!(!readable(&deadline.0));
    }
}
