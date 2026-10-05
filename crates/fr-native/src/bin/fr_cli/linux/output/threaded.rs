#![forbid(unsafe_code)]
//! Session-facing proxy. Every `PulseAudio` operation, including construction and
//! destruction, belongs to one foreign-work thread, not the session executor.
//! The restricted decoder process and its existing retirement proof are retained.
use super::{Report, end_reason, now};
use fr_client::input::ClientInstant;
use fr_core::audio::{AudioDirection, AudioGeneration, MAX_OPUS_PAYLOAD_BYTES};
use fr_wire::audio::{self as wire, AudioConfiguration};
use frd::session_startup::{ViewerAudioEnd, ViewerAudioOutput, ViewerAudioRefused};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const PACKETS: usize = 8;
const RECORD_BYTES: usize = wire::AUDIO_PACKET_OVERHEAD + MAX_OPUS_PAYLOAD_BYTES;
// A periodically refreshed observation snapshot, NOT an input lease. A stopped
// or expired snapshot cannot be revived. Drop/end/reset synchronously fence it.
const PERMISSION_US: u64 = 100_000;
const STALL_US: u64 = 250_000;
const TICK: Duration = Duration::from_millis(1);
const CLEANUP: Duration = Duration::from_secs(2);
const ACK_PENDING: u8 = 0;
const ACK_ACCEPTED: u8 = 1;
type Ack = [u8; wire::AUDIO_CONFIGURED_RECORD_BYTES];

// An uninterruptible foreign call must not allow reconnects to accumulate
// abandoned threads/devices. Only proven native/child cleanup releases this slot.
static WORKER_SLOT: OnceLock<Arc<AtomicBool>> = OnceLock::new();

#[derive(Clone, Copy)]
#[repr(u8)]
enum Fault {
    Native = 1,
    Stalled,
    Permission,
    Backpressure,
    Acknowledgement,
}
fn reason(code: u8) -> &'static str {
    match code {
        2 => "audio_output_worker_stalled",
        3 => "audio_output_permission_expired",
        4 => "audio_output_backpressure",
        5 => "audio_output_acknowledgement_failed",
        _ => "audio_output_failed",
    }
}

struct Shared {
    stopped: AtomicBool,
    until: AtomicU64,
    progress: AtomicU64,
    fault: AtomicU8,
    ack: AtomicU8,
    submitted: AtomicU64,
    retired: AtomicBool,
}
impl Shared {
    fn new(now: u64) -> Self {
        Self {
            stopped: AtomicBool::new(false),
            until: AtomicU64::new(0),
            progress: AtomicU64::new(now),
            fault: AtomicU8::new(0),
            ack: AtomicU8::new(ACK_PENDING),
            submitted: AtomicU64::new(0),
            retired: AtomicBool::new(false),
        }
    }
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }
    fn fail(&self, fault: Fault) {
        let _ = self
            .fault
            .compare_exchange(0, fault as u8, Ordering::AcqRel, Ordering::Acquire);
        self.stop();
    }
    fn live_at(&self, now: u64) -> bool {
        !self.stopped.load(Ordering::Acquire) && now < self.until.load(Ordering::Acquire)
    }
    fn authorize_at(&self, now: u64) -> Result<(), ViewerAudioRefused> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(ViewerAudioRefused);
        }
        let old = self.until.load(Ordering::Acquire);
        let Some(until) = now.checked_add(PERMISSION_US) else {
            self.fail(Fault::Permission);
            return Err(ViewerAudioRefused);
        };
        if old != 0 && now >= old {
            self.fail(Fault::Permission);
            return Err(ViewerAudioRefused);
        }
        if now.saturating_sub(self.progress.load(Ordering::Acquire)) >= STALL_US {
            self.fail(Fault::Stalled);
            return Err(ViewerAudioRefused);
        }
        self.until.store(until, Ordering::Release);
        if self.stopped.load(Ordering::Acquire) {
            return Err(ViewerAudioRefused);
        }
        Ok(())
    }
}

// Fixed-size records make the inbox byte bound independent of allocator capacity:
// PACKETS * size_of::<Packet>(), plus one producer and one worker-local packet.
struct Packet {
    arrived: ClientInstant,
    len: usize,
    bytes: [u8; RECORD_BYTES],
}
impl Packet {
    fn new(bytes: &[u8], arrived: ClientInstant) -> Result<Self, ViewerAudioRefused> {
        if bytes.len() > RECORD_BYTES {
            return Err(ViewerAudioRefused);
        }
        let mut packet = Self {
            arrived,
            len: bytes.len(),
            bytes: [0; RECORD_BYTES],
        };
        packet.bytes[..bytes.len()].copy_from_slice(bytes);
        Ok(packet)
    }
}
struct Job {
    shared: Arc<Shared>,
    packets: SyncSender<Packet>,
    replies: Receiver<Ack>,
    thread: JoinHandle<()>,
    acknowledged: bool,
    counted: u64,
}
impl Job {
    fn spawn(
        slot: &Arc<AtomicBool>,
        shared: Arc<Shared>,
        work: impl FnOnce(&Shared, Receiver<Packet>, SyncSender<Ack>) -> bool + Send + 'static,
    ) -> Result<Self, ViewerAudioRefused> {
        slot.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| ViewerAudioRefused)?;
        let (packets, inbox) = mpsc::sync_channel(PACKETS);
        let (replies, outbox) = mpsc::sync_channel(1);
        let worker = shared.clone();
        let worker_slot = slot.clone();
        let thread = thread::Builder::new()
            .name("fr-audio-output".into())
            .spawn(move || {
                let mut exit = Exit {
                    shared: worker,
                    slot: worker_slot,
                    clean: false,
                };
                exit.clean = work(&exit.shared, inbox, replies);
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(_) => {
                slot.store(false, Ordering::Release);
                return Err(ViewerAudioRefused);
            }
        };
        Ok(Self {
            shared,
            packets,
            replies: outbox,
            thread,
            acknowledged: false,
            counted: 0,
        })
    }
    fn stop(&self) {
        self.shared.stop();
        self.thread.thread().unpark();
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        self.stop(); // Never join or run a foreign destructor on the session thread.
    }
}
struct Exit {
    shared: Arc<Shared>,
    slot: Arc<AtomicBool>,
    clean: bool,
}
impl Drop for Exit {
    fn drop(&mut self) {
        self.shared.stop();
        if self.clean {
            self.shared.retired.store(true, Ordering::Release);
            self.slot.store(false, Ordering::Release);
        } else {
            // Panic/unknown cleanup deliberately poisons admission for this CLI.
            self.shared.fail(Fault::Native);
        }
    }
}

pub(crate) struct Output {
    server: PathBuf,
    sink: Option<String>,
    image: Option<PathBuf>,
    origin: Instant,
    job: Option<Job>,
    current: Option<(u32, AudioConfiguration)>,
    last: Option<AudioGeneration>,
    ended: bool,
    report: Rc<RefCell<Report>>,
}
impl Output {
    pub(crate) fn new(server: PathBuf, sink: Option<String>, report: Rc<RefCell<Report>>) -> Self {
        Self {
            server,
            sink,
            image: super::super::decoder_image().ok(),
            origin: Instant::now(),
            job: None,
            current: None,
            last: None,
            ended: false,
            report,
        }
    }
    fn account(&mut self) {
        if let Some(job) = &mut self.job {
            let submitted = job.shared.submitted.load(Ordering::Acquire);
            let mut report = self.report.borrow_mut();
            report.submitted = report
                .submitted
                .saturating_add(submitted.saturating_sub(job.counted));
            job.counted = submitted;
            let fault = job.shared.fault.load(Ordering::Acquire);
            if fault != 0 {
                report.absent(reason(fault));
            }
        }
    }
    fn authorize(&self, live: &mut dyn FnMut() -> bool) -> Result<(), ViewerAudioRefused> {
        let job = self.job.as_ref().ok_or(ViewerAudioRefused)?;
        if !live() {
            job.stop();
            return Err(ViewerAudioRefused);
        }
        job.shared.authorize_at(now(self.origin).0)?;
        job.thread.thread().unpark();
        Ok(())
    }
}
impl ViewerAudioOutput for Output {
    fn configure(
        &mut self,
        binding: u32,
        offer: AudioConfiguration,
    ) -> Result<(), ViewerAudioRefused> {
        self.account();
        if self.ended
            || self.current.is_some()
            || binding == 0
            || offer.direction != AudioDirection::Downlink
            || self.last.is_some_and(|last| offer.generation <= last)
            || offer.validate().is_err()
            || self.job.as_ref().is_some_and(|job| {
                !job.thread.is_finished() || !job.shared.retired.load(Ordering::Acquire)
            })
        {
            return Err(ViewerAudioRefused);
        }
        let image = self.image.clone().ok_or(ViewerAudioRefused)?;
        let config = NativeConfig {
            server: self.server.clone(),
            sink: self.sink.clone(),
            image,
            origin: self.origin,
            binding,
            offer,
        };
        let shared = Arc::new(Shared::new(now(self.origin).0));
        let slot = WORKER_SLOT
            .get_or_init(|| Arc::new(AtomicBool::new(false)))
            .clone();
        let job = Job::spawn(&slot, shared, move |shared, packets, replies| {
            native(config, shared, &packets, &replies)
        })
        .inspect_err(|_| {
            self.report
                .borrow_mut()
                .absent("audio_output_worker_unavailable");
        })?;
        self.job = Some(job);
        self.last = Some(offer.generation);
        self.current = Some((binding, offer));
        Ok(())
    }
    fn service(
        &mut self,
        live: &mut dyn FnMut() -> bool,
        acknowledge: &mut dyn FnMut(&[u8]) -> Result<(), ViewerAudioRefused>,
    ) -> Result<(), ViewerAudioRefused> {
        self.account();
        if self.current.is_none() {
            return Ok(()); // Cleanup is owned by the worker, not renewed here.
        }
        self.authorize(live)?;
        let job = self.job.as_mut().ok_or(ViewerAudioRefused)?;
        match job.replies.try_recv() {
            Ok(record) => {
                if job.acknowledged || !job.shared.live_at(now(self.origin).0) || !live() {
                    job.shared.fail(Fault::Acknowledgement);
                    return Err(ViewerAudioRefused);
                }
                if acknowledge(&record).is_err() {
                    job.shared.fail(Fault::Acknowledgement);
                    return Err(ViewerAudioRefused);
                }
                // Actual local transport acceptance, not insertion into the worker outbox.
                job.acknowledged = true;
                self.report.borrow_mut().acknowledged = true;
                if !live() || !job.shared.live_at(now(self.origin).0) {
                    job.stop();
                    return Err(ViewerAudioRefused);
                }
                job.shared.ack.store(ACK_ACCEPTED, Ordering::Release);
                job.thread.thread().unpark();
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                job.shared.fail(Fault::Native);
                return Err(ViewerAudioRefused);
            }
        }
        Ok(())
    }
    fn receive(
        &mut self,
        bytes: &[u8],
        live: &mut dyn FnMut() -> bool,
    ) -> Result<(), ViewerAudioRefused> {
        let arrived = now(self.origin);
        let (binding, offer) = self.current.ok_or(ViewerAudioRefused)?;
        // A stop is never enqueued behind media, even when the inbox is full.
        if let Ok(stop) = wire::decode_stop(bytes, binding) {
            if stop.direction == offer.direction && stop.generation == offer.generation {
                self.ended(ViewerAudioEnd::Host(stop.reason));
                return Ok(());
            }
            return Err(ViewerAudioRefused);
        }
        self.authorize(live)?;
        let job = self.job.as_ref().ok_or(ViewerAudioRefused)?;
        let valid = wire::decode_packet(bytes, binding).is_ok_and(|packet| {
            job.acknowledged
                && packet.direction == offer.direction
                && packet.generation == offer.generation
                && packet.payload.len() <= usize::try_from(offer.max_packet_bytes).unwrap_or(0)
                && u32::from(packet.duration_samples) <= offer.max_decoded_samples
        });
        if !valid {
            job.shared.fail(Fault::Native);
            return Err(ViewerAudioRefused);
        }
        let packet = Packet::new(bytes, arrived)?;
        if job.packets.try_send(packet).is_err() {
            job.shared.fail(Fault::Backpressure);
            return Err(ViewerAudioRefused);
        }
        job.thread.thread().unpark();
        Ok(())
    }
    fn ended(&mut self, end: ViewerAudioEnd) {
        if let Some(job) = &self.job {
            job.stop();
        }
        self.current = None;
        self.ended = true;
        self.account();
        self.report.borrow_mut().absent(end_reason(end));
    }
    fn reset(&mut self) {
        if let Some(job) = &self.job {
            job.stop();
        }
        self.current = None;
        self.account();
        let mut report = self.report.borrow_mut();
        report.resets = report.resets.saturating_add(1);
        // Keep the original job until its thread AND decoder have retired.
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        if let Some(job) = &self.job {
            job.stop();
        }
        self.account();
    }
}

struct NativeConfig {
    server: PathBuf,
    sink: Option<String>,
    image: PathBuf,
    origin: Instant,
    binding: u32,
    offer: AudioConfiguration,
}
// The only wait in the acknowledgement bridge is on the foreign-work thread.
// PulsePlayout::acknowledge cannot return success until the session accepted its
// exact bytes. Cancellation and the ORIGINAL setup deadline still fence it.
fn forward_ack(
    shared: &Shared,
    origin: Instant,
    replies: &SyncSender<Ack>,
    bytes: &[u8],
) -> Result<(), ViewerAudioRefused> {
    let record: Ack = bytes.try_into().map_err(|_| ViewerAudioRefused)?;
    if !shared.live_at(now(origin).0) || replies.try_send(record).is_err() {
        return Err(ViewerAudioRefused);
    }
    let until = Instant::now() + Duration::from_micros(PERMISSION_US);
    while shared.live_at(now(origin).0) && Instant::now() < until {
        if shared.ack.load(Ordering::Acquire) == ACK_ACCEPTED {
            return Ok(());
        }
        thread::park_timeout(TICK);
    }
    shared.fail(Fault::Acknowledgement);
    Err(ViewerAudioRefused)
}
fn native(
    config: NativeConfig,
    shared: &Shared,
    packets: &Receiver<Packet>,
    replies: &SyncSender<Ack>,
) -> bool {
    let report = Rc::new(RefCell::new(Report::default()));
    let mut output = super::Output::new(config.server, config.sink, report.clone());
    output.image = Some(config.image);
    output.origin = config.origin;
    let result = (|| {
        // configure() cannot receive the observation callback. Wait for the first
        // session service turn instead of assuming creation grants permission.
        let until = Instant::now() + CLEANUP;
        while shared.until.load(Ordering::Acquire) == 0 {
            if shared.stopped.load(Ordering::Acquire) || Instant::now() >= until {
                return Err(ViewerAudioRefused);
            }
            thread::park_timeout(TICK);
        }
        if !shared.live_at(now(config.origin).0) {
            return Err(ViewerAudioRefused);
        }
        output.configure(config.binding, config.offer)?;
        loop {
            if shared.stopped.load(Ordering::Acquire) {
                return Ok(());
            }
            if !shared.live_at(now(config.origin).0) {
                shared.fail(Fault::Permission);
                return Err(ViewerAudioRefused);
            }
            output.service(
                &mut || shared.live_at(now(config.origin).0),
                &mut |bytes| forward_ack(shared, config.origin, replies, bytes),
            )?;
            shared
                .progress
                .store(now(config.origin).0, Ordering::Release);
            shared
                .submitted
                .store(report.borrow().submitted, Ordering::Release);
            for _ in 0..PACKETS {
                match packets.try_recv() {
                    Ok(packet) => output.receive_at(
                        &packet.bytes[..packet.len],
                        packet.arrived,
                        &mut || shared.live_at(now(config.origin).0),
                    )?,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            }
            thread::park_timeout(TICK);
        }
    })();
    if result.is_err() && !shared.stopped.load(Ordering::Acquire) {
        shared.fail(Fault::Native);
    }
    shared.stop();
    // All foreign cleanup stays here. On timeout, the process-wide slot remains
    // unavailable instead of permitting an unbounded population of leaked owners.
    let until = Instant::now() + CLEANUP;
    output.ended(ViewerAudioEnd::Local);
    while matches!(output.stage, super::Stage::Stopping(_)) && Instant::now() < until {
        let _ = output.service(&mut || false, &mut |_| Err(ViewerAudioRefused));
        thread::park_timeout(TICK);
    }
    output.reset();
    let retirement = output.retirement.clone();
    drop(output);
    while retirement.as_ref().is_some_and(|r| !r.is_complete()) {
        if Instant::now() >= until {
            return false;
        }
        thread::park_timeout(TICK);
    }
    true
}

#[cfg(test)]
mod tests;
