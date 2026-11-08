//! A signed installed-app metadata bridge; no desktop or network authority.
//!
//! The whole native child, its output and its reap owner live on one drained
//! blocking task. The one-second lookup deadline starts before queue admission.
use asupersync::{cx::Cx, time::timeout_at, types::Time};
use fr_tailnet::{Discovery, Error};
use std::{
    future::{Future, poll_fn},
    pin::pin,
    sync::atomic::{AtomicBool, Ordering},
    task::Poll,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

mod native;

static BUSY: AtomicBool = AtomicBool::new(false);
struct Permit;
impl Drop for Permit {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}

#[derive(Clone)]
struct Lookup {
    owner: Cx,
    start: Time,
    end: Time,
    previous: Time,
    elapsed: Instant,
}
impl Lookup {
    fn new(cx: &Cx) -> Result<Self, Error> {
        let elapsed = Instant::now();
        let start = now(cx)?;
        Ok(Self {
            owner: cx.clone(),
            start,
            end: Time::from_nanos(
                start
                    .as_nanos()
                    .checked_add(1_000_000_000)
                    .ok_or(Error::Clock)?,
            ),
            previous: start,
            elapsed,
        })
    }
    fn check(&mut self) -> Result<Time, Error> {
        let current = now(&self.owner)?;
        if current < self.previous {
            return Err(Error::Clock);
        }
        self.previous = current;
        if current >= self.end || self.elapsed.elapsed() >= Duration::from_secs(1) {
            return Err(Error::Timeout);
        }
        Ok(current)
    }
}
fn now(cx: &Cx) -> Result<Time, Error> {
    cx.checkpoint().map_err(|_| Error::Cancelled)?;
    Ok(cx.timer_driver().ok_or(Error::MissingRuntime)?.now())
}

/// Read one bounded status snapshot through the installed Tailscale app's
/// machine-readable CLI. The exact suspended child must pass Apple's dynamic
/// code-signature check before it runs. A snapshot never authorizes a session.
/// No user-supplied path, argument, proxy, credential or environment is used.
///
/// Cancellation/drop retains the original child and the global admission slot
/// until the blocking task has killed/reaped it; a late result cannot publish.
pub async fn discover(cx: &Cx) -> Result<Discovery, Error> {
    let mut lookup = Lookup::new(cx)?;
    let wall = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Clock)?
            .as_micros(),
    )
    .map_err(|_| Error::Clock)?;
    BUSY.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| Error::Busy)?;
    let permit = Permit;
    let mut worker_lookup = lookup.clone();
    let mut task = cx
        .spawn_blocking_drained(move |child| {
            // Declared first: the slot cannot reopen before the native child drops.
            let _permit = permit;
            let _ambient = child.clone().set_current_restricted();
            let started_us = worker_lookup.start.as_nanos() / 1000;
            let mut check = || {
                child.checkpoint().map_err(|_| Error::Cancelled)?;
                worker_lookup.check()
            };
            check()?;
            let mut snapshot = native::Snapshot::start()?;
            // Signing can block. Only the original live budget can resume this child.
            check()?;
            snapshot.resume()?;
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(Discovery::MAX_STATUS_BYTES)
                .map_err(|_| Error::MalformedMetadata)?;
            let mut buffer = [0; 8192];
            let mut eof = false;
            loop {
                check()?;
                let progress = match snapshot.read(&mut buffer)? {
                    native::Read::Bytes(n) => {
                        if n > Discovery::MAX_STATUS_BYTES - bytes.len() {
                            return Err(Error::MalformedMetadata);
                        }
                        bytes.extend_from_slice(&buffer[..n]);
                        true
                    }
                    native::Read::Eof => {
                        eof = true;
                        false
                    }
                    native::Read::Pending => false,
                };
                match snapshot.try_wait()? {
                    Some(false) => return Err(Error::LocalApiDenied),
                    Some(true) if eof => break,
                    _ => {}
                }
                // Keep a full pipe moving; idle polls cannot occupy the async reactor.
                if !progress {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            drop(snapshot); // Actual reap precedes accepting or publishing metadata.
            check()?;
            Discovery::from_status_json(&bytes, started_us, wall, || {
                check().map(|time| time.as_nanos() / 1000)
            })
        })
        .map_err(|_| Error::MissingRuntime)?;
    let result = {
        let mut cancelled = pin!(cx.cancelled());
        let mut joined = pin!(timeout_at(lookup.end, task.join()));
        poll_fn(|context| {
            lookup.check()?;
            if cancelled.as_mut().poll(context).is_ready() {
                return Poll::Ready(Err(Error::Cancelled));
            }
            let polled = joined.as_mut().poll(context);
            lookup.check()?;
            match polled {
                Poll::Ready(Ok(Ok(value))) => Poll::Ready(value),
                Poll::Ready(Ok(Err(asupersync::runtime::JoinError::Cancelled(_)))) => {
                    Poll::Ready(Err(Error::Cancelled))
                }
                Poll::Ready(Ok(Err(_))) => Poll::Ready(Err(Error::LocalApiUnavailable)),
                Poll::Ready(Err(_)) => Poll::Ready(Err(Error::Timeout)),
                Poll::Pending => Poll::Pending,
            }
        })
        .await
    };
    if result.is_err() && !task.is_finished() {
        // This command owns a short-lived runtime. Returning before retirement
        // could exit the process with a suspended child. Keep the original
        // refusal while collecting actual native cleanup; no work deadline or
        // metadata validity is extended by this cancellation drain.
        task.abort();
        let _ = task.join().await;
    }
    result
}
