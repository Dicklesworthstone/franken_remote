//! A separate fixed, content-free stop channel. EOF, any byte, or any I/O
//! failure permanently fences the ORIGINAL application, even while its native
//! worker is waiting on the command channel. No lease or clipboard bytes cross
//! this channel. The owning native worker closes and joins this monitor.
use crate::native_clipboard::Control;
use fr_core::clipboard::PlatformError;
use std::{
    io::{self, Read},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

struct State {
    stopped: AtomicBool,
    original: Option<Control>,
}
impl State {
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(original) = &self.original {
            original.stop();
        }
    }
}
struct Fence(Arc<State>);
impl Drop for Fence {
    fn drop(&mut self) {
        self.0.stop();
    }
}

pub(super) struct Monitor {
    stream: UnixStream,
    state: Arc<State>,
    task: Option<JoinHandle<()>>,
}
impl Monitor {
    pub(super) fn start(
        stream: UnixStream,
        original: Option<Control>,
    ) -> Result<Self, PlatformError> {
        let state = Arc::new(State {
            stopped: AtomicBool::new(false),
            original,
        });
        let mut reader = stream.try_clone().map_err(|_| {
            state.stop();
            PlatformError::Unavailable
        })?;
        reader
            .set_read_timeout(Some(Duration::from_millis(50)))
            .map_err(|_| {
                state.stop();
                PlatformError::Unavailable
            })?;
        let shared = state.clone();
        let task = thread::Builder::new()
            .name("fr-clipboard-privacy".into())
            .spawn(move || {
                // Panic and normal completion have the same terminal fence.
                let _fence = Fence(shared.clone());
                let mut fixed = [0_u8; 1];
                while !shared.stopped.load(Ordering::Acquire) {
                    match reader.read(&mut fixed) {
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::Interrupted
                                    | io::ErrorKind::TimedOut
                                    | io::ErrorKind::WouldBlock
                            ) => {}
                        _ => return,
                    }
                }
            })
            .map_err(|_| {
                state.stop();
                PlatformError::Unavailable
            })?;
        Ok(Self {
            stream,
            state,
            task: Some(task),
        })
    }
    pub(super) fn stopped(&self) -> bool {
        self.state.stopped.load(Ordering::Acquire)
    }
    pub(super) fn stop(&self) {
        self.state.stop();
        // Wakes the original reader even if the child or its X server hangs.
        let _ = self.stream.shutdown(Shutdown::Both);
    }
    pub(super) fn finish(&mut self) {
        self.stop();
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        clipboard_process::RemoteClipboard,
        native_clipboard::{Application, Configuration},
    };
    use std::{io::Write, time::Instant};

    fn application() -> Application {
        Application::new(
            Configuration::new::<RemoteClipboard, _, _>(
                true,
                Duration::from_secs(1),
                || Err(PlatformError::Unavailable),
                || Ok(1),
            )
            .unwrap(),
        )
    }
    fn wait(mut done: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(1);
        while !done() {
            assert!(Instant::now() < until, "privacy monitor did not fence");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn byte_and_eof_stop_only_the_original_owner_and_cannot_reenable() {
        for send_byte in [false, true] {
            let app = application();
            let foreign = application();
            let original = app.control();
            let (ours, mut child) = UnixStream::pair().unwrap();
            let mut monitor = Monitor::start(ours, Some(original.clone())).unwrap();
            if send_byte {
                child.write_all(&[0]).unwrap();
            } else {
                child.shutdown(Shutdown::Both).unwrap();
            }
            wait(|| !original.status().enabled);
            assert!(monitor.stopped());
            assert!(foreign.control().status().enabled);
            assert_eq!(
                original.set_enabled(true),
                Err(crate::clipboard_quic::Error::Closed)
            );
            monitor.finish();
            assert!(monitor.task.is_none());
        }
    }

    #[test]
    fn cleanup_wakes_and_joins_the_reader_without_child_cooperation() {
        let app = application();
        let (ours, _child_held_open) = UnixStream::pair().unwrap();
        let mut monitor = Monitor::start(ours, Some(app.control())).unwrap();
        let started = Instant::now();
        monitor.finish();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(monitor.task.is_none());
        assert!(!app.control().status().enabled);
        monitor.finish();
    }
}
