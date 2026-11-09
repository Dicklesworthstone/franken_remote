//! The one thread that forks every worker child.
//!
//! A Linux worker asks for SIGKILL when its parent dies (`PR_SET_PDEATHSIG`,
//! fr-native's `bind_parent`), and Linux ties that request to the THREAD that
//! forked it. The launch runs on Asupersync's drained blocking pool, whose
//! threads retire after 10 s idle, so every worker was killed about 10 s after
//! its fork (a live 5120x2880 viewer: SIGKILL 9.997 s after the worker
//! started, no kill syscall). This thread lives as long as the process, so a
//! worker dies with frd or fr, never with a pool thread. Jobs run one at a
//! time: each is one fork/exec.
use super::Error;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Mutex, PoisonError,
        mpsc::{self, Sender},
    },
    thread,
};

type Job = Box<dyn FnOnce() + Send>;

static LAUNCHER: Mutex<Option<Sender<Job>>> = Mutex::new(None);

/// Run `job` on the process-lifetime launcher thread and wait for its result.
/// The caller blocks: call it only from blocking work, never an async poll.
pub(super) fn run<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> Result<T, Error> {
    let sender = {
        let mut launcher = LAUNCHER.lock().unwrap_or_else(PoisonError::into_inner);
        if launcher.is_none() {
            let (sender, jobs) = mpsc::channel::<Job>();
            thread::Builder::new()
                .name("fr-worker-launcher".into())
                .spawn(move || {
                    // Never exits while the process runs: the static sender
                    // is never dropped. A panicking job loses only its reply.
                    for job in jobs {
                        let _ = catch_unwind(AssertUnwindSafe(job));
                    }
                })
                .map_err(|_| Error::Unavailable)?;
            *launcher = Some(sender);
        }
        launcher.as_ref().ok_or(Error::Unavailable)?.clone()
    };
    let (reply, result) = mpsc::sync_channel(1);
    sender
        .send(Box::new(move || {
            let _ = reply.send(job());
        }))
        .map_err(|_| Error::Unavailable)?;
    result.recv().map_err(|_| Error::Unavailable)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use asupersync::{process::Command, runtime::RuntimeBuilder, types::Budget};
    use std::time::Duration;

    /// A child that asks for SIGKILL on parent death, launched from a thread
    /// that then exits (as a retiring blocking-pool thread does), survives.
    #[test]
    fn a_child_outlives_the_thread_that_requested_its_launch() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let (child, requester) = std::thread::spawn(move || {
            let child = super::run(move || {
                let _ambient = cx.set_current_restricted();
                Command::new("sleep")
                    .arg("30")
                    .kill_on_drop(true)
                    .parent_death_signal(Some(9))
                    .spawn()
            });
            (child, std::thread::current().id())
        })
        .join()
        .unwrap();
        let mut child = child.unwrap().unwrap();
        assert_ne!(requester, std::thread::current().id());
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            child.try_wait().unwrap().is_none(),
            "the child died with the thread that requested it"
        );
        child.kill().unwrap();
        let _ = child.wait();
    }
}
