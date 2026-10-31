//! Same-user local inspection, observation and stop of ONE foreground owner.
//! The abstract socket owns no daemon, PID, input lease, transport or retry.
//! Only the original foreground supervisor can confirm its native cleanup.
#![forbid(unsafe_code)]

#[path = "session_control/input.rs"]
pub(super) mod input;
#[path = "session_control/observation.rs"]
pub(super) mod observation;
#[path = "session_control/screenshot.rs"]
pub(super) mod screenshot;
#[cfg(any(feature = "linux-desktop", test))]
use asupersync::net::unix::UnixListener;
use asupersync::{
    cx::Cx,
    io::{AsyncReadExt, AsyncWriteExt},
    net::unix::UnixStream,
    time::{TimerDriverHandle, TimerHandle},
    types::Time,
};
#[cfg(any(feature = "linux-desktop", test))]
use std::pin::Pin;
use std::{
    fmt::{self, Write as _},
    future::{Future, poll_fn},
    io,
    pin::pin,
    task::{Context, Poll, Waker},
    time::Duration,
};

const MAX_TARGET: usize = 253;
#[cfg(any(feature = "linux-desktop", test))]
const MAX_PEERS: usize = 4;
const HANDSHAKE: Duration = Duration::from_secs(2);
const COMPLETION: Duration = Duration::from_secs(30);
const IDENTIFY: [u8; 8] = *b"FRSC1ID\0";
const IDENTITY: [u8; 8] = *b"FRSC1ME\0";
const STOP: [u8; 8] = *b"FRSC1ST\0";
const FINISHED: [u8; 8] = *b"FRSC1FN\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    InvalidTarget,
    InvalidInstance,
    #[cfg_attr(not(any(feature = "linux-desktop", test)), allow(dead_code))]
    AlreadyRunning,
    NoSession,
    Unavailable,
    UntrustedPeer,
    Protocol,
    WrongOwner,
    Timeout,
    /// A stop or input request may have been accepted without a confirmed reply.
    OutcomeUnknown,
    Cancelled,
}
impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "local-session-control: {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Info {
    pub target: String,
    /// Public comparison identifier, never a credential or input lease.
    pub instance: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Completion {
    /// LOCAL cleanup, not host-held-key release or rollback of prior effects.
    Confirmed,
    Unconfirmed,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Stopped {
    pub info: Info,
    pub completion: Completion,
}

#[cfg(any(feature = "linux-desktop", test))]
type Peer = Pin<Box<dyn Future<Output = Result<Option<UnixStream>, Error>> + Send>>;

/// Four bounded pending handshakes and one stop requester; no unbounded queue.
#[cfg(any(feature = "linux-desktop", test))]
pub(super) struct Owner {
    listener: UnixListener,
    uid: u32,
    target: String,
    instance: [u8; 16],
    peers: Vec<Peer>,
    requester: Option<UnixStream>,
    completed: bool,
    observations: Option<observation::Publisher>,
    automation: Option<input::Shared>,
}
#[cfg(any(feature = "linux-desktop", test))]
impl Owner {
    pub async fn bind(cx: &Cx, target: &str) -> Result<Self, Error> {
        check_target(target)?;
        cx.checkpoint().map_err(|_| Error::Cancelled)?;
        let uid = own_uid()?;
        let address = name(uid, target);
        let listener = match UnixListener::bind_abstract(&address).await {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                // Public socket occupancy is not proof of a same-user owner.
                bounded(cx, Deadline::new(cx, HANDSHAKE)?, async {
                    let stream = UnixStream::connect_abstract(&address)
                        .await
                        .map_err(|_| Error::Unavailable)?;
                    check_peer(&stream, uid)
                })
                .await?;
                return Err(Error::AlreadyRunning);
            }
            Err(_) => return Err(Error::Unavailable),
        };
        let mut instance = [0; 16];
        cx.random_bytes(&mut instance);
        Ok(Self {
            listener,
            uid,
            target: target.to_owned(),
            instance,
            peers: Vec::with_capacity(MAX_PEERS),
            requester: None,
            completed: false,
            observations: None,
            automation: None,
        })
    }
    pub fn with_observations(mut self, publisher: observation::Publisher) -> Self {
        self.observations = Some(publisher);
        self
    }
    /// Explicit opt-in only. This opaque LOCAL handle is a comparison fence;
    /// the original controlled viewer retains every host lease and bearer ticket.
    #[cfg(feature = "linux-desktop")]
    pub fn enable_input(&mut self, cx: &Cx) -> Result<input::Shared, Error> {
        if self.automation.is_some() || self.observations.is_some() {
            return Err(Error::Unavailable);
        }
        let mut lease = [0; 16];
        cx.random_bytes(&mut lease);
        let shared = input::Shared::new(self.instance, lease)?;
        self.automation = Some(shared.clone());
        Ok(shared)
    }
    pub fn info(&self) -> Info {
        Info {
            target: self.target.clone(),
            instance: hex(&self.instance),
        }
    }
    /// Poll beside the original session, never instead of its cleanup. Snapshot
    /// readers use the same credential checks, capacity and original deadline.
    pub async fn wait_for_stop(&mut self, cx: &Cx) -> Result<(), Error> {
        poll_fn(|task| {
            cx.checkpoint().map_err(|_| Error::Cancelled)?;
            if self.completed {
                return Poll::Ready(Err(Error::Unavailable));
            }
            if self.requester.is_some() {
                return Poll::Ready(Ok(()));
            }
            let mut accept_budget_spent = true;
            for _ in 0..MAX_PEERS {
                match self.listener.poll_accept(task) {
                    Poll::Ready(Ok((stream, _))) => {
                        if self.peers.len() == MAX_PEERS || check_peer(&stream, self.uid).is_err() {
                            continue;
                        }
                        let deadline = Deadline::new(cx, HANDSHAKE)?;
                        let clock = cx.clone();
                        let identity = identity_record(&self.target, self.instance);
                        let instance = self.instance;
                        let observations = self.observations.clone();
                        let automation = self.automation.clone();
                        self.peers.push(Box::pin(async move {
                            bounded(
                                &clock,
                                deadline,
                                peer(stream, identity, instance, observations, automation),
                            )
                            .await
                        }));
                    }
                    Poll::Ready(Err(_)) => return Poll::Ready(Err(Error::Unavailable)),
                    Poll::Pending => {
                        accept_budget_spent = false;
                        break;
                    }
                }
            }
            let mut index = 0;
            while index < self.peers.len() {
                match self.peers[index].as_mut().poll(task) {
                    Poll::Ready(result) => {
                        drop(self.peers.swap_remove(index));
                        if let Ok(Some(stream)) = result {
                            if let Some(automation) = &self.automation {
                                automation.retire();
                            }
                            self.requester = Some(stream);
                            self.peers.clear();
                            if let Some(publisher) = &self.observations {
                                publisher.retire(fr_client::robot::live::Phase::Cleaning);
                            }
                            return Poll::Ready(Ok(()));
                        }
                    }
                    Poll::Pending => index += 1,
                }
            }
            // A fifth queued caller still needs a bounded turn when the first
            // four completed without leaving a registered socket/timer waiter.
            if accept_budget_spent {
                task.waker().wake_by_ref();
            }
            Poll::Pending
        })
        .await
    }
    /// Only call after the original supervisor finishes, on an independently
    /// live cleanup Cx. A lost requester cannot change the collected outcome.
    pub async fn complete(&mut self, cleanup: &Cx, result: Completion) -> Result<(), Error> {
        if let Some(automation) = &self.automation {
            automation.retire();
        }
        self.completed = true;
        self.peers.clear();
        if let Some(publisher) = &self.observations {
            publisher.retire(fr_client::robot::live::Phase::Stopped);
        }
        let Some(mut stream) = self.requester.take() else {
            return Ok(());
        };
        let mut record = [0; 25];
        record[..8].copy_from_slice(&FINISHED);
        record[8..24].copy_from_slice(&self.instance);
        record[24] = u8::from(result == Completion::Confirmed);
        bounded(cleanup, Deadline::new(cleanup, HANDSHAKE)?, async {
            stream
                .write_all(&record)
                .await
                .map_err(|_| Error::Unavailable)
        })
        .await
    }
}

/// Identifies a local invocation, not remote readiness or a control grant.
pub(super) async fn probe(cx: &Cx, target: &str) -> Result<Info, Error> {
    let (_, info, _) = identified(cx, target).await?;
    Ok(info)
}
pub(super) async fn stop(cx: &Cx, target: &str) -> Result<Stopped, Error> {
    stop_checked(cx, target, None).await
}
/// Authenticate and compare on the SAME socket before sending STOP. Never
/// reconnect or repeat an uncertain request against a replacement owner.
pub(super) async fn stop_if(cx: &Cx, target: &str, expected: &str) -> Result<Stopped, Error> {
    if expected.len() != 32
        || !expected
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::InvalidInstance);
    }
    stop_checked(cx, target, Some(expected)).await
}
async fn stop_checked(cx: &Cx, target: &str, expected: Option<&str>) -> Result<Stopped, Error> {
    let (mut stream, info, instance) = identified(cx, target).await?;
    if expected.is_some_and(|expected| expected != info.instance) {
        return Err(Error::WrongOwner);
    }
    let mut request = [0; 24];
    request[..8].copy_from_slice(&STOP);
    request[8..].copy_from_slice(&instance);
    // Once writing is attempted, loss/cancellation cannot be a definite refusal.
    let result = bounded(cx, Deadline::new(cx, COMPLETION)?, async {
        stream
            .write_all(&request)
            .await
            .map_err(|_| Error::OutcomeUnknown)?;
        let mut response = [0; 25];
        stream
            .read_exact(&mut response)
            .await
            .map_err(|_| Error::OutcomeUnknown)?;
        if response[..8] != FINISHED || response[8..24] != instance {
            return Err(Error::OutcomeUnknown);
        }
        let completion = match response[24] {
            0 => Completion::Unconfirmed,
            1 => Completion::Confirmed,
            _ => return Err(Error::OutcomeUnknown),
        };
        Ok(Stopped { info, completion })
    })
    .await;
    result.map_err(|_| Error::OutcomeUnknown)
}
async fn identified(cx: &Cx, target: &str) -> Result<(UnixStream, Info, [u8; 16]), Error> {
    check_target(target)?;
    let uid = own_uid()?;
    bounded(cx, Deadline::new(cx, HANDSHAKE)?, async {
        let mut stream = UnixStream::connect_abstract(&name(uid, target))
            .await
            .map_err(|error| match error.kind() {
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound => Error::NoSession,
                _ => Error::Unavailable,
            })?;
        check_peer(&stream, uid)?;
        stream
            .write_all(&IDENTIFY)
            .await
            .map_err(|_| Error::Unavailable)?;
        let mut header = [0; 26];
        stream
            .read_exact(&mut header)
            .await
            .map_err(|_| Error::Unavailable)?;
        if header[..8] != IDENTITY {
            return Err(Error::Protocol);
        }
        let mut instance = [0; 16];
        instance.copy_from_slice(&header[8..24]);
        let length = usize::from(u16::from_be_bytes([header[24], header[25]]));
        if length == 0 || length > MAX_TARGET {
            return Err(Error::Protocol);
        }
        let mut bytes = [0; MAX_TARGET];
        stream
            .read_exact(&mut bytes[..length])
            .await
            .map_err(|_| Error::Unavailable)?;
        if &bytes[..length] != target.as_bytes() {
            return Err(Error::WrongOwner);
        }
        let info = Info {
            target: target.to_owned(),
            instance: hex(&instance),
        };
        Ok((stream, info, instance))
    })
    .await
}
#[cfg(any(feature = "linux-desktop", test))]
async fn peer(
    mut stream: UnixStream,
    identity: Vec<u8>,
    instance: [u8; 16],
    observations: Option<observation::Publisher>,
    automation: Option<input::Shared>,
) -> Result<Option<UnixStream>, Error> {
    let mut identify = [0; 8];
    stream
        .read_exact(&mut identify)
        .await
        .map_err(|_| Error::Protocol)?;
    if identify != IDENTIFY {
        return Err(Error::Protocol);
    }
    stream
        .write_all(&identity)
        .await
        .map_err(|_| Error::Protocol)?;
    let mut request = [0; 24];
    stream
        .read_exact(&mut request)
        .await
        .map_err(|_| Error::Protocol)?;
    if request[8..] != instance {
        return Err(Error::WrongOwner);
    }
    if request[..8] == STOP {
        return Ok(Some(stream));
    }
    if request[..8] == input::STATUS || request[..8] == input::SUBMIT {
        input::respond(
            &mut stream,
            instance,
            request[..8] == input::SUBMIT,
            automation.as_ref(),
        )
        .await?;
        return Ok(None);
    }
    if request[..8] == observation::REQUEST {
        observation::respond(&mut stream, instance, observations.as_ref()).await?;
        return Ok(None);
    }
    if request[..8] == screenshot::REQUEST {
        screenshot::respond(
            &mut stream,
            instance,
            observations.as_ref(),
            automation.as_ref(),
        )
        .await?;
        return Ok(None);
    }
    Err(Error::Protocol)
}
fn check_target(target: &str) -> Result<(), Error> {
    if target.is_empty() || target.len() > MAX_TARGET || target.chars().any(char::is_control) {
        return Err(Error::InvalidTarget);
    }
    Ok(())
}
fn own_uid() -> Result<u32, Error> {
    UnixStream::pair()
        .and_then(|(stream, _)| stream.peer_cred())
        .map(|credentials| credentials.uid)
        .map_err(|_| Error::Unavailable)
}
fn check_peer(stream: &UnixStream, uid: u32) -> Result<(), Error> {
    match stream.peer_cred() {
        Ok(credentials) if credentials.uid == uid => Ok(()),
        _ => Err(Error::UntrustedPeer),
    }
}
fn name(uid: u32, target: &str) -> Vec<u8> {
    let digest = fr_client::robot::sha256::sha256_hex(target.as_bytes());
    format!("frankenremote.connect.v1.{uid}.{digest}").into_bytes()
}
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}
#[cfg(any(feature = "linux-desktop", test))]
fn identity_record(target: &str, instance: [u8; 16]) -> Vec<u8> {
    let mut record = Vec::with_capacity(26 + target.len());
    record.extend_from_slice(&IDENTITY);
    record.extend_from_slice(&instance);
    record.extend_from_slice(
        &u16::try_from(target.len())
            .expect("validated target bound")
            .to_be_bytes(),
    );
    record.extend_from_slice(target.as_bytes());
    record
}

/// Original absolute deadlines survive repolling and changing wakers.
struct Deadline {
    clock: TimerDriverHandle,
    until: Time,
    previous: Time,
    timer: Option<(TimerHandle, Waker)>,
}
impl Deadline {
    fn new(cx: &Cx, duration: Duration) -> Result<Self, Error> {
        cx.checkpoint().map_err(|_| Error::Cancelled)?;
        let clock = cx.timer_driver().ok_or(Error::Unavailable)?;
        let previous = clock.now();
        let nanos = u64::try_from(duration.as_nanos()).map_err(|_| Error::Unavailable)?;
        let until = Time::from_nanos(
            previous
                .as_nanos()
                .checked_add(nanos)
                .ok_or(Error::Unavailable)?,
        );
        Ok(Self {
            clock,
            until,
            previous,
            timer: None,
        })
    }
    fn check(&mut self, cx: &Cx, task: &Context<'_>) -> Result<(), Error> {
        cx.checkpoint().map_err(|_| Error::Cancelled)?;
        let now = self.clock.now();
        if now < self.previous {
            return Err(Error::Unavailable);
        }
        self.previous = now;
        if now >= self.until {
            return Err(Error::Timeout);
        }
        if self
            .timer
            .as_ref()
            .is_none_or(|(_, waker)| !waker.will_wake(task.waker()))
        {
            if let Some((handle, _)) = self.timer.take() {
                let _ = self.clock.cancel(&handle);
            }
            self.timer = Some((
                self.clock.register(self.until, task.waker().clone()),
                task.waker().clone(),
            ));
        }
        Ok(())
    }
}
impl Drop for Deadline {
    fn drop(&mut self) {
        if let Some((handle, _)) = self.timer.take() {
            let _ = self.clock.cancel(&handle);
        }
    }
}
async fn bounded<T>(
    cx: &Cx,
    mut deadline: Deadline,
    work: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    let mut work = pin!(work);
    poll_fn(|task| {
        deadline.check(cx, task)?;
        match work.as_mut().poll(task) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                deadline.check(cx, task)?;
                Poll::Ready(result)
            }
        }
    })
    .await
}

#[cfg(test)]
#[path = "linux/session_control/inspection_tests.rs"]
mod inspection_tests;
#[cfg(test)]
#[path = "linux/session_control/tests.rs"]
mod tests;
