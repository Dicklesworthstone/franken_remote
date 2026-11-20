//! An explicit permission-only invocation of the exact signed capture helper.
//! This process never receives a device or speech request. Its positive result
//! is published only after actual exit; the caller still needs a fresh Talk.
#![forbid(unsafe_code)]

use super::{
    Error, Slot,
    permission_protocol::{Reply, Request},
};
use std::{
    io::{Read, Write},
    os::{
        fd::OwnedFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct Shared {
    stopped: AtomicBool,
    complete: AtomicBool,
    result: Mutex<Option<Result<(), Error>>>,
    check: Arc<dyn Fn() -> bool + Send + Sync>,
    request: Request,
}
impl Shared {
    fn live(&self) -> bool {
        !self.stopped.load(Ordering::Acquire)
            && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.check)()))
                .unwrap_or(false)
            && !self.stopped.load(Ordering::Acquire)
            && crate::clock::monotonic_ns().is_some_and(|now| now < self.request.wait_until_ns)
    }
}

/// Private worker role. Only a bounded content-free result is returned over
/// the already validated original private socket; no source is selected.
pub fn run(parent: u32) -> Result<(), Error> {
    let result = crate::macos_microphone_capture::authorize(parent);
    let reply = match result {
        Ok(()) => Reply::Granted,
        Err(Error::Denied) => Reply::Denied,
        Err(Error::Configuration | Error::Protocol) => Reply::Configuration,
        Err(Error::Expired | Error::Clock) => Reply::Expired,
        Err(_) => Reply::Unavailable,
    };
    std::io::stdout()
        .lock()
        .write_all(&[reply as u8])
        .map_err(|_| Error::Protocol)?;
    result
}

/// Retains the exact helper's cleanup even after the visible owner stops.
pub struct Permission(Arc<Shared>);
impl Permission {
    pub fn start(
        image: &Path,
        launch_until_ns: u64,
        wait_until_ns: u64,
        check: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Result<Self, Error> {
        if !image.is_absolute() || !image.is_file() {
            return Err(Error::Spawn);
        }
        let request = Request::new(launch_until_ns, wait_until_ns).ok_or(Error::Protocol)?;
        if !crate::clock::monotonic_ns().is_some_and(|now| request.may_launch(now)) {
            return Err(Error::Expired);
        }
        let slot = Slot::take()?;
        let shared = Arc::new(Shared {
            stopped: AtomicBool::new(false),
            complete: AtomicBool::new(false),
            result: Mutex::new(None),
            check: Arc::new(check),
            request,
        });
        let owner = shared.clone();
        let image = image.to_owned();
        std::thread::Builder::new()
            .name("fr-mic-permission".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    serve(&image, &owner, slot)
                }))
                .unwrap_or(Err(Error::Native));
                if let Ok(mut output) = owner.result.lock() {
                    *output = Some(result);
                }
                // serve returns only after reap, or before a child exists. A failed
                // wait permanently retains the admission slot and leaves pending.
            })
            .map_err(|_| Error::Spawn)?;
        Ok(Self(shared))
    }
    pub fn stop(&self) {
        self.0.stopped.store(true, Ordering::Release);
    }
    pub fn is_complete(&self) -> bool {
        self.0.complete.load(Ordering::Acquire)
    }
    pub fn result(&self) -> Option<Result<(), Error>> {
        if !self.is_complete() {
            return None;
        }
        self.0.result.lock().ok().and_then(|result| *result)
    }
}
impl Drop for Permission {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Owner<'a> {
    child: Child,
    shared: &'a Shared,
    slot: Option<Slot>,
}
impl Drop for Owner<'_> {
    fn drop(&mut self) {
        let _ = self.child.kill();
        if self.child.wait().is_ok() {
            self.shared.complete.store(true, Ordering::Release);
        } else if let Some(slot) = self.slot.take() {
            std::mem::forget(slot);
        }
    }
}

fn serve(image: &Path, shared: &Shared, slot: Slot) -> Result<(), Error> {
    let setup = (|| {
        if !shared.live()
            || !crate::clock::monotonic_ns().is_some_and(|now| shared.request.may_launch(now))
        {
            return Err(Error::Denied);
        }
        let (socket, child_socket) = UnixStream::pair().map_err(|_| Error::Spawn)?;
        let stdout: OwnedFd = child_socket.try_clone().map_err(|_| Error::Spawn)?.into();
        let (guard, child_guard) = UnixStream::pair().map_err(|_| Error::Spawn)?;
        let stdin: OwnedFd = child_socket.into();
        let stderr: OwnedFd = child_guard.into();
        socket.set_nonblocking(true).map_err(|_| Error::Spawn)?;
        let child = Command::new(image)
            .env_clear()
            .args([
                "--authorize",
                "--parent-pid",
                &std::process::id().to_string(),
            ])
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .process_group(0)
            .spawn()
            .map_err(|_| Error::Spawn)?;
        Ok((socket, guard, child))
    })();
    let (mut socket, guard, child) = match setup {
        Ok(started) => started,
        Err(error) => {
            shared.complete.store(true, Ordering::Release);
            return Err(error);
        }
    };
    let mut owner = Owner {
        child,
        shared,
        slot: Some(slot),
    };
    let result = (|| {
        if !shared.live()
            || !crate::clock::monotonic_ns().is_some_and(|now| shared.request.may_launch(now))
        {
            return Err(Error::Expired);
        }
        socket
            .write_all(&shared.request.encode())
            .map_err(|_| Error::Protocol)?;
        let mut received = None;
        loop {
            if !shared.live() {
                return Err(Error::Denied);
            }
            let mut bytes = [0; 2];
            match socket.read(&mut bytes) {
                Ok(1) if received.is_none() => {
                    received = Some(Reply::decode(bytes[0]).ok_or(Error::Protocol)?);
                }
                Ok(0) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                _ => return Err(Error::Protocol),
            }
            if let Some(status) = owner.child.try_wait().map_err(|_| Error::Spawn)? {
                if received.is_none() {
                    // Inspect bytes written just before exit, without creating
                    // any replacement request or extending the original cap.
                    if matches!(socket.read(&mut bytes), Ok(1)) {
                        received = Reply::decode(bytes[0]);
                    }
                }
                if !matches!(socket.read(&mut bytes), Ok(0)) {
                    return Err(Error::Protocol);
                }
                if !shared.live() {
                    return Err(Error::Denied);
                }
                return match received {
                    Some(Reply::Granted) if status.success() => Ok(()),
                    Some(Reply::Denied) if !status.success() => Err(Error::Denied),
                    Some(Reply::Configuration) if !status.success() => Err(Error::Configuration),
                    Some(Reply::Expired) if !status.success() => Err(Error::Expired),
                    Some(Reply::Unavailable) if !status.success() => Err(Error::Unavailable),
                    _ => Err(Error::Protocol),
                };
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    drop(guard); // terminal native permission fence before kill/reap
    drop(owner);
    result
}
