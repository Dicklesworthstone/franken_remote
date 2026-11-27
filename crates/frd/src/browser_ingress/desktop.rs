//! One approved, source-owned browser observation over two WSS connections.
//!
//! The listener retains the exact worker retirement independently of both
//! connection futures. A socket, nonce or ticket never creates observation:
//! the registered host factory supplies an already approved original authority.
use super::{
    BrowserIngress, BrowserSessionMetrics, bounded_startup, first_message_auth_duration, http,
    websocket,
};
use crate::{
    broker::browser_security::{AuxiliaryChannelRole, BrowserSessionRole, FetchMetadata},
    media::{CaptureSource, ObservationControl, host_now},
    worker::{self, Launch, Retirement},
};
use asupersync::{
    cx::Cx,
    io::{AsyncRead, AsyncWrite, ReadBuf},
    time::sleep_until,
    types::Time,
};
use fr_core::{
    ids::{RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration},
    time::HostInstant,
};
use fr_media::worker::Configuration;
use fr_tailnet::Lease;
use fr_transport::wss::{ChannelGeneration, ReceiverCreditWindow, WssProfile};
use fr_wire::{
    decoder,
    display::{Catalog, Select},
    input::{InputDelivery as Delivery, InputDirection as Direction},
    negotiation::{ControlBinding, Message, Role, Selection, browser},
    refusal::{self, Reason, Refused},
};
use ring::rand::{SecureRandom, SystemRandom};
use std::{
    collections::VecDeque,
    fmt,
    future::{Future, poll_fn},
    io,
    pin::{Pin, pin},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
    time::Duration,
};

mod control;
mod video;

/// Called only after exact-origin, one-use nonce and canonical selection checks.
/// This is the actual accepted TCP lease, including both original endpoints.
pub struct Request {
    pub peer: Lease,
    pub cx: Cx,
    pub binding: ControlBinding,
    pub selection: Selection,
}
pub type SelectSource = Box<dyn FnOnce(&Catalog) -> Result<(Select, Configuration), ()> + Send>;
pub type Supervisor = Pin<Box<dyn Future<Output = Result<(), ()>> + Send>>;
/// Local host ownership, not data manufactured by the browser ingress.
pub struct Authorized {
    pub control: ObservationControl,
    pub launch: Launch,
    pub select: SelectSource,
    pub cleanup: Cx,
    pub supervise: Supervisor,
}
pub type Factory = Arc<
    dyn Fn(Request) -> Pin<Box<dyn Future<Output = Result<Authorized, Reason>> + Send>>
        + Send
        + Sync,
>;

pub(super) struct Service {
    factory: Factory,
    slot: Mutex<Slot>,
    closed: AtomicBool,
}
#[derive(Default)]
struct Slot {
    preparing: bool,
    session: Option<Arc<Session>>,
}
impl fmt::Debug for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BrowserDesktopService([original authority and worker])")
    }
}
impl Service {
    pub(super) fn new(factory: Factory) -> Self {
        Self {
            factory,
            slot: Mutex::new(Slot::default()),
            closed: AtomicBool::new(false),
        }
    }
    pub(super) fn available(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
            && self
                .slot
                .lock()
                .is_ok_and(|s| !s.preparing && s.session.is_none())
    }
    fn reserve(&self) -> io::Result<Reservation<'_>> {
        let mut s = self.slot.lock().map_err(|_| failed())?;
        if self.closed.load(Ordering::Acquire) || s.preparing || s.session.is_some() {
            return Err(failed());
        }
        s.preparing = true;
        Ok(Reservation {
            service: self,
            committed: false,
        })
    }
    fn session(&self) -> io::Result<Arc<Session>> {
        self.slot
            .lock()
            .map_err(|_| failed())?
            .session
            .clone()
            .ok_or_else(failed)
    }
    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let session = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session
            .clone();
        if let Some(session) = session {
            // Waking a transport must never occur under the admission lock.
            session.close();
        }
    }
    pub(super) fn has_closed(&self) -> bool {
        self.session().is_ok_and(|session| {
            session.closed.load(Ordering::Acquire)
                && !session.reaped.load(Ordering::Acquire)
                && session.retirement.lock().is_ok_and(|owner| owner.is_some())
        })
    }
    pub(super) async fn retire_closed(&self, cleanup: &Cx) -> io::Result<()> {
        let session = self.slot.lock().map_err(|_| failed())?.session.clone();
        let Some(session) = session else {
            return Ok(());
        };
        if !session.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        match self.retire_session(&session, cleanup).await {
            // A concurrent original control cleanup keeps its receipt. The
            // listener can check has_closed again after that future releases it.
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
            result => result,
        }
    }
    /// The original child owner survives future drop. A cancellation during
    /// reaping restores that same Retirement into this slot, never a PID.
    pub(super) async fn retire(&self, cleanup: &Cx) -> io::Result<()> {
        // External shutdown cannot race a newly admitted replacement.
        self.close();
        let session = self.slot.lock().map_err(|_| failed())?.session.clone();
        let Some(session) = session else {
            return Ok(());
        };
        self.retire_session(&session, cleanup).await
    }
    async fn retire_session(&self, session: &Arc<Session>, cleanup: &Cx) -> io::Result<()> {
        // Normal completion names its EXACT original owner. An old socket may
        // never look up and close a replacement that won the next admission.
        if session.reaped.load(Ordering::Acquire) {
            return Ok(());
        }
        session.close();
        session.prepared.lock().map_err(|_| failed())?.take();
        let retirement = session.retirement.lock().map_err(|_| failed())?.take();
        let Some(retirement) = retirement else {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "original browser cleanup is already owned",
            ));
        };
        let mut retained = RetainedRetirement {
            slot: &session.retirement,
            original: Some(retirement),
            complete: false,
        };
        let deadline =
            worker::Deadline::after(cleanup, Duration::from_secs(2)).map_err(|_| failed())?;
        loop {
            match retained
                .original
                .as_mut()
                .ok_or_else(failed)?
                .reap(cleanup, deadline)
                .await
            {
                Ok(_) => break,
                Err(worker::Error::ReapPending) => {
                    cleanup.checkpoint().map_err(|_| failed())?;
                    let now = host_now(cleanup).map_err(|_| failed())?.as_micros();
                    if Time::from_nanos(now.saturating_mul(1000)) >= deadline.time() {
                        return Err(failed());
                    }
                    sleep_until(
                        Time::from_nanos(now.saturating_add(5_000).saturating_mul(1000))
                            .min(deadline.time()),
                    )
                    .await;
                }
                Err(_) => return Err(failed()),
            }
        }
        retained.complete = true;
        session.reaped.store(true, Ordering::Release);
        let mut slot = self.slot.lock().map_err(|_| failed())?;
        if slot
            .session
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            slot.session.take();
        }
        Ok(())
    }
    pub(super) fn metrics(&self, id: RemoteSessionId) -> Option<BrowserSessionMetrics> {
        let session = self.session().ok()?;
        if session.binding.remote_session != id {
            return None;
        }
        let state = session.state.lock().ok()?;
        Some(BrowserSessionMetrics {
            session_id: id,
            role: BrowserSessionRole::Observer,
            video_generation: ChannelGeneration(1),
            video_attached: state.video_attached,
            audio_attached: false,
            outstanding_video_bytes: state.credit.outstanding_bytes(),
            outstanding_video_frames: state.credit.outstanding_frames(),
            credit_epoch: 1,
            transport_label: WssProfile::new(false).diagnostic_label(),
        })
    }
}
struct Reservation<'a> {
    service: &'a Service,
    committed: bool,
}
impl Reservation<'_> {
    fn install(mut self, session: Arc<Session>) -> io::Result<()> {
        let mut s = self.service.slot.lock().map_err(|_| failed())?;
        if self.service.closed.load(Ordering::Acquire) || !s.preparing || s.session.is_some() {
            session.close();
            return Err(failed());
        }
        s.session = Some(session);
        s.preparing = false;
        self.committed = true;
        Ok(())
    }
}
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if !self.committed
            && let Ok(mut s) = self.service.slot.lock()
        {
            s.preparing = false;
        }
    }
}
struct RetainedRetirement<'a> {
    slot: &'a Mutex<Option<Retirement>>,
    original: Option<Retirement>,
    complete: bool,
}
impl Drop for RetainedRetirement<'_> {
    fn drop(&mut self) {
        if !self.complete {
            // Poison cannot discard the only original cleanup owner.
            *self
                .slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = self.original.take();
        }
    }
}
struct Prepared {
    source: CaptureSource,
    configuration: Configuration,
}
struct State {
    accepted: bool,
    descriptor: Option<browser::attachment::Descriptor>,
    grant: Option<browser::attachment::Grant>,
    binding_accepted: bool,
    video_attached: bool,
    running: bool,
    pending_challenge: bool,
    next_challenge_us: u64,
    credit: ReceiverCreditWindow,
    outgoing: VecDeque<(u8, Vec<u8>)>,
}
struct Session {
    binding: ControlBinding,
    selection: Selection,
    byte_allowance: u64,
    picture_allowance: u32,
    peer: Lease,
    control: ObservationControl,
    cleanup: Cx,
    supervisor: Mutex<Option<Supervisor>>,
    state: Mutex<State>,
    prepared: Mutex<Option<Prepared>>,
    retirement: Mutex<Option<Retirement>>,
    closed: AtomicBool,
    startup_until_us: u64,
    reaped: AtomicBool,
    parked: Mutex<[Option<Waker>; 2]>,
    writing: [AtomicBool; 2],
}
impl Session {
    fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.control.revoke();
            let parked = std::mem::take(
                &mut *self
                    .parked
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            for waker in parked.into_iter().flatten() {
                waker.wake();
            }
        }
    }
    fn check(&self) -> io::Result<u64> {
        if self.closed.load(Ordering::Acquire) {
            return Err(failed());
        }
        let now = self.control.check().map_err(|_| failed())?.as_micros();
        let running = self.state.lock().map_err(|_| failed())?.running;
        if !running && now >= self.startup_until_us {
            return Err(expired());
        }
        Ok(now)
    }
    fn poll_check(&self, task: &mut Context<'_>, lane: usize) -> io::Result<u64> {
        // Exactly two original connection owners. Revocation wakes BOTH even
        // when TLS/worker I/O would otherwise remain parked indefinitely.
        let replacement = task.waker().clone();
        let previous = {
            let mut parked = self.parked.lock().map_err(|_| failed())?;
            parked[lane].replace(replacement)
        };
        drop(previous);
        // Every resumed socket and worker operation drives the original local
        // supervisor first, including a stop discovered while I/O was parked.
        let ended = {
            let mut owner = self.supervisor.lock().map_err(|_| failed())?;
            if let Some(future) = owner.as_mut() {
                if future.as_mut().poll(task).is_ready() {
                    owner.take();
                    true
                } else {
                    false
                }
            } else {
                true
            }
        };
        if ended {
            self.close();
            return Err(failed());
        }
        let result = self.check();
        if result.is_err() {
            self.close();
        }
        result
    }
    async fn run<T>(
        &self,
        lane: usize,
        work: impl Future<Output = io::Result<T>>,
    ) -> io::Result<T> {
        let mut work = pin!(work);
        poll_fn(|task| {
            if let Err(error) = self.poll_check(task, lane) {
                return Poll::Ready(Err(error));
            }
            work.as_mut().poll(task)
        })
        .await
    }
    fn enqueue(&self, opcode: u8, bytes: Vec<u8>) -> io::Result<()> {
        if bytes.len() > browser::MAX_RECORD {
            return Err(failed());
        }
        let mut state = self.state.lock().map_err(|_| failed())?;
        if state.outgoing.len() >= 8 {
            return Err(failed());
        }
        state.outgoing.push_back((opcode, bytes));
        Ok(())
    }
    fn begin_write(&self, lane: usize) -> io::Result<WriteCustody<'_>> {
        self.check()?;
        self.writing[lane]
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| failed())?;
        Ok(WriteCustody {
            session: self,
            lane,
            flushed: false,
        })
    }
    async fn pause(&self, micros: u64) -> io::Result<()> {
        let now = self.check()?;
        sleep_until(Time::from_nanos(
            now.saturating_add(micros).saturating_mul(1000),
        ))
        .await;
        self.check()?;
        Ok(())
    }
}
/// The pinned TLS implementation can flush queued ciphertext from poll_read.
/// Until a whole WebSocket frame has been flushed, only its original writer
/// may poll TLS: media writes keep the PacketOffer's final owner and deadline.
/// Abandoning a partial frame fences the session before reads become possible.
struct WriteCustody<'a> {
    session: &'a Session,
    lane: usize,
    flushed: bool,
}
impl WriteCustody<'_> {
    fn flushed(mut self) {
        self.flushed = true;
    }
}
impl Drop for WriteCustody<'_> {
    fn drop(&mut self) {
        if !self.flushed {
            self.session.close();
        }
        self.session.writing[self.lane].store(false, Ordering::Release);
        let parked = self
            .session
            .parked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)[self.lane]
            .take();
        if let Some(waker) = parked {
            waker.wake();
        }
    }
}
/// Fences even if a caller drops or unwinds the complete connection future.
/// It does not claim cleanup: the separate Retirement slot proves the exit.
struct AuthorizationGuard(Option<ObservationControl>);
impl Drop for AuthorizationGuard {
    fn drop(&mut self) {
        if let Some(control) = self.0.take() {
            control.revoke();
        }
    }
}
struct SessionGuard(Arc<Session>);
impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.close();
        self.0
            .prepared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

/// One original transport, with every resumed read/write/flush checked before
/// touching TLS. Ingress-only instances also guard HTTP/bootstrap I/O.
pub(super) struct GuardedIo<S> {
    stream: S,
    peer: Lease,
    cx: Cx,
    session: Option<Arc<Session>>,
    until_us: Option<u64>,
    lane: usize,
    alarm: Option<(u64, Pin<Box<dyn Future<Output = ()> + Send>>)>,
}
impl<S> GuardedIo<S> {
    pub(super) fn ingress(stream: S, peer: Lease, cx: Cx) -> Self {
        Self {
            stream,
            peer,
            cx,
            session: None,
            until_us: None,
            lane: 0,
            alarm: None,
        }
    }
    fn session(stream: S, peer: Lease, session: Arc<Session>, lane: usize) -> Self {
        Self {
            stream,
            peer,
            cx: session.control.context(),
            session: Some(session),
            until_us: None,
            lane,
            alarm: None,
        }
    }
    fn gate(&mut self, task: &mut Context<'_>) -> io::Result<()> {
        self.cx.checkpoint().map_err(|_| failed())?;
        self.peer.observe().map_err(|_| failed())?;
        let now = if let Some(session) = &self.session {
            if !self.peer.same_peer(&session.peer).map_err(|_| failed())? {
                session.close();
                return Err(failed());
            }
            session.poll_check(task, self.lane)?
        } else {
            host_now(&self.cx).map_err(|_| failed())?.as_micros()
        };
        if let Some(until) = self.until_us {
            if now >= until {
                return Err(expired());
            }
            if self
                .alarm
                .as_ref()
                .is_none_or(|(original, _)| *original != until)
            {
                self.alarm = Some((
                    until,
                    Box::pin(sleep_until(Time::from_nanos(
                        until.checked_mul(1000).ok_or_else(expired)?,
                    ))),
                ));
            }
            if self
                .alarm
                .as_mut()
                .ok_or_else(expired)?
                .1
                .as_mut()
                .poll(task)
                .is_ready()
            {
                return Err(expired());
            }
        } else {
            self.alarm = None;
        }
        Ok(())
    }
}
impl<S: AsyncRead + Unpin> AsyncRead for GuardedIo<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        bytes: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.gate(task) {
            return Poll::Ready(Err(error));
        }
        if this
            .session
            .as_ref()
            .is_some_and(|session| session.writing[this.lane].load(Ordering::Acquire))
        {
            // A read can also pump TLS output. Park it behind the original
            // writer's packet/deadline gate until that exact frame is flushed.
            return Poll::Pending;
        }
        Pin::new(&mut this.stream).poll_read(task, bytes)
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for GuardedIo<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Err(error) = this.gate(task) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.stream).poll_write(task, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.gate(task) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.stream).poll_flush(task)
    }
    fn poll_shutdown(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.gate(task) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.stream).poll_shutdown(task)
    }
}

fn failed() -> io::Error {
    io::Error::other("browser desktop session ended")
}
fn expired() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "browser desktop deadline expired")
}
fn credit_limits(
    config: &super::BrowserIngressConfig,
    selection: &Selection,
) -> Result<(u64, u32), Reason> {
    let bytes = config
        .max_video_credit_bytes
        .min(browser::MAX_VIDEO_CREDIT_BYTES)
        .min(selection.limits.per_viewer_compressed_bytes());
    let frames = u32::try_from(
        config
            .max_video_credit_frames
            .min(u64::from(browser::MAX_VIDEO_CREDIT_FRAMES)),
    )
    .map_err(|_| Reason::InvalidLimits)?;
    let required = browser::frame_charge(selection.limits.max_encoded_access_unit_bytes())
        .map_err(|_| Reason::InvalidLimits)?;
    if frames == 0 || bytes < required {
        return Err(Reason::InvalidLimits);
    }
    Ok((bytes, frames))
}
pub(super) fn random_id() -> io::Result<u128> {
    let mut bytes = [0; 16];
    SystemRandom::new().fill(&mut bytes).map_err(|_| failed())?;
    let id = u128::from_be_bytes(bytes);
    if id == 0 {
        return Err(failed());
    }
    Ok(id)
}
fn encode_message(message: &Message, maximum: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; maximum.min(browser::MAX_RECORD)];
    let length = browser::encode(message, maximum, &mut bytes).map_err(|_| failed())?;
    bytes.truncate(length);
    Ok(bytes)
}
fn encode_attachment(
    message: browser::attachment::Message,
    session: &Session,
) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; browser::attachment::GRANT_RECORD_BYTES];
    let length = browser::attachment::encode(
        message,
        session.binding,
        &session.selection.limits,
        &mut bytes,
        Direction::HostToViewer,
        Delivery::Reliable,
    )
    .map_err(|_| failed())?;
    bytes.truncate(length);
    Ok(bytes)
}
async fn refuse<S: AsyncWrite + Unpin>(stream: &mut S, reason: Reason) -> io::Result<()> {
    let mut bytes = [0; refusal::MAX_BYTES];
    let length = refusal::encode(
        Refused::connection(reason),
        0,
        browser::MAX_RECORD,
        &mut bytes,
        Delivery::Reliable,
    )
    .map_err(|_| failed())?;
    // Refusals can still own the single preparation reservation. A peer that
    // stops reading cannot retain that slot indefinitely without any session.
    bounded_startup(first_message_auth_duration(), async {
        websocket::write_server_binary(stream, &bytes[..length]).await?;
        websocket::write_server_close(stream, 1008).await
    })
    .await
}

pub(super) async fn serve_control<S: AsyncRead + AsyncWrite + Unpin>(
    ingress: &BrowserIngress,
    stream: &mut S,
    req: &http::HttpRequest,
    metadata: &FetchMetadata<'_>,
    peer: Lease,
    cx: Cx,
    sni: Option<&str>,
) -> io::Result<()> {
    control::serve(ingress, stream, req, metadata, peer, cx, sni).await
}
pub(super) async fn serve_video<S: AsyncRead + AsyncWrite + Unpin>(
    ingress: &BrowserIngress,
    stream: &mut S,
    req: &http::HttpRequest,
    metadata: &FetchMetadata<'_>,
    peer: Lease,
    sni: Option<&str>,
) -> io::Result<()> {
    video::serve(ingress, stream, req, metadata, peer, sni).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::{
        io::{AsyncReadExt, AsyncWriteExt, duplex},
        runtime::RuntimeBuilder,
    };
    use fr_core::ids::{HostBootId, OsSessionId};
    use std::sync::atomic::AtomicUsize;

    fn unavailable_factory(calls: Arc<AtomicUsize>) -> Factory {
        Arc::new(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(Reason::PermissionDenied) })
        })
    }

    #[test]
    fn one_capture_preparation_slot_survives_refusal_and_stops_new_admission() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = Service::new(unavailable_factory(calls.clone()));
        assert!(service.available());
        let reservation = service.reserve().unwrap();
        assert!(!service.available());
        assert!(service.reserve().is_err());
        drop(reservation);
        assert!(service.available());
        let reservation = service.reserve().unwrap();
        service.close();
        drop(reservation);
        assert!(!service.available());
        assert!(service.reserve().is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn configured_credit_ceiling_cannot_be_expanded_by_browser_negotiation() {
        let selection = browser::select(&browser::offer(browser::limits())).unwrap();
        let mut config = super::super::BrowserIngressConfig::https_tailnet(
            "host.example.ts.net",
            8443,
            HostBootId::from_raw(1),
            OsSessionId::from_raw(2),
            browser::limits(),
        );
        assert_eq!(
            credit_limits(&config, &selection).unwrap(),
            (
                browser::MAX_VIDEO_CREDIT_BYTES,
                browser::MAX_VIDEO_CREDIT_FRAMES
            )
        );
        config.max_video_credit_frames = 1;
        assert_eq!(credit_limits(&config, &selection).unwrap().1, 1);
        config.max_video_credit_bytes =
            browser::frame_charge(selection.limits.max_encoded_access_unit_bytes()).unwrap() - 1;
        assert_eq!(
            credit_limits(&config, &selection),
            Err(Reason::InvalidLimits)
        );
        config.max_video_credit_bytes = browser::MAX_VIDEO_CREDIT_BYTES;
        config.max_video_credit_frames = 0;
        assert_eq!(
            credit_limits(&config, &selection),
            Err(Reason::InvalidLimits)
        );
    }

    #[test]
    fn registered_factory_cannot_activate_unverified_bootstrap_transport() {
        let runtime = RuntimeBuilder::new()
            .worker_threads(1)
            .blocking_threads(1, 2)
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            let calls = Arc::new(AtomicUsize::new(0));
            let ingress = BrowserIngress::new(super::super::BrowserIngressConfig::https_tailnet(
                "host.example.ts.net", 8443, HostBootId::from_raw(1), OsSessionId::from_raw(2),
                fr_core::limits::ProtocolLimits::ABSOLUTE,
            )).with_desktop(unavailable_factory(calls.clone()));
            let body = br#"{"role":"observer"}"#;
            let request = format!(
                "POST /api/session/bootstrap HTTP/1.1\r\nHost: host.example.ts.net:8443\r\nOrigin: https://host.example.ts.net:8443\r\nSec-Fetch-Site: same-origin\r\nContent-Length: {}\r\n\r\n",
                body.len());
            let (server, mut client) = duplex(8192);
            client.write_all(request.as_bytes()).await.unwrap();
            client.write_all(body).await.unwrap();
            ingress.serve_stream(server, "100.64.0.2".parse().unwrap(),
                Some("host.example.ts.net"), || HostInstant::from_micros(1)).await.unwrap();
            let mut bytes = Vec::new();
            client.read_to_end(&mut bytes).await.unwrap();
            let response = String::from_utf8(bytes).unwrap();
            assert!(response.starts_with("HTTP/1.1 200"));
            assert!(response.contains("\"desktop_available\":false"));
            assert!(!response.contains("\"nonce_hex\""));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(ingress.policy.with_nonce_manager(|m| m.pending_count()).unwrap(), 0);
        });
    }

    #[test]
    fn abandoning_cleanup_wrapper_preserves_original_unlaunched_receipt() {
        let runtime = RuntimeBuilder::new()
            .worker_threads(1)
            .blocking_threads(1, 2)
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            let cleanup = Cx::current().unwrap();
            let (launch, original) = Launch::new(
                std::path::Path::new("/unused/capture-worker"),
                ":0",
                None,
                fr_media::worker::Role::Capture,
                1,
            )
            .unwrap()
            .retain_cleanup()
            .unwrap();
            let slot = Mutex::new(Some(original));
            {
                let held = slot.lock().unwrap().take();
                let _interrupted = RetainedRetirement {
                    slot: &slot,
                    original: held,
                    complete: false,
                };
            }
            assert!(slot.lock().unwrap().is_some());
            drop(launch); // Positive proof this original one-use launch never started.
            let mut original = slot.lock().unwrap().take().unwrap();
            assert!(
                original
                    .reap(
                        &cleanup,
                        worker::Deadline::after(&cleanup, Duration::from_secs(1)).unwrap()
                    )
                    .await
                    .unwrap()
                    .is_none()
            );
        });
    }
}
