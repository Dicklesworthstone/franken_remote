//! Narrow same-thread WASAPI render/COM boundary. No callback retains Rust data.
use super::{Error, Retirement};
use crate::{
    macos_audio_policy::Window,
    windows_audio_policy::{self as policy, SleepOffset, Tail, Write},
};
use std::{
    marker::PhantomData,
    rc::Rc,
    sync::atomic::{AtomicUsize, Ordering},
};
use windows::{
    Win32::{
        Media::Audio::{
            AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE, IAudioClient,
            IAudioClock, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
            WAVEFORMATEX, eConsole, eRender,
        },
        System::{
            Com::{
                CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
                CoUninitialize,
            },
            Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
            WindowsProgramming::{QueryInterruptTimePrecise, QueryUnbiasedInterruptTimePrecise},
        },
    },
    core::Interface,
};

const BUFFER_100NS: i64 = 400_000;
const MAX_ENDPOINT_UNITS: usize = 1024;
const MAX_OWNERS: usize = 4;
static OWNERS: AtomicUsize = AtomicUsize::new(0);

struct Slot {
    release: bool,
}
impl Slot {
    fn acquire() -> Result<Self, Error> {
        OWNERS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_OWNERS).then_some(count + 1)
            })
            .map_err(|_| Error::Backpressure)?;
        Ok(Self { release: true })
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        if self.release {
            OWNERS.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

struct Com(PhantomData<Rc<()>>);
impl Com {
    fn new() -> Result<Self, Error> {
        // SAFETY: this worker thread owns the matching guard. Both S_OK and
        // S_FALSE require one CoUninitialize; failed initialization does not.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|_| Error::Native)?;
        Ok(Self(PhantomData))
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: guard is !Send; every interface is released before this field.
        unsafe { CoUninitialize() };
    }
}

struct Interfaces {
    render: IAudioRenderClient,
    clock: IAudioClock,
    client: IAudioClient,
    device: IMMDevice,
    enumerator: IMMDeviceEnumerator,
    _com: Com,
}

#[derive(Clone, Copy)]
pub(super) struct Reading {
    pub samples: u64,
    padding: u32,
}

pub(super) struct Render {
    inner: Option<Interfaces>,
    id: Vec<u16>,
    channels: u8,
    capacity: u32,
    latency: i64,
    frequency: u64,
    qpc_frequency: u64,
    sleep: SleepOffset,
    lead: u64,
    idle_lead: u64,
    tail: Tail,
    pcm_started: bool,
    closed: Option<Result<(), Error>>,
    retirement: Retirement,
    slot: Slot,
}

impl Render {
    pub fn open(channels: u8, largest: u32, retirement: Retirement) -> Result<Self, Error> {
        if !matches!(channels, 1 | 2) {
            return Err(Error::Configuration);
        }
        let slot = Slot::acquire()?;
        let com = Com::new()?;
        // SAFETY: initialized original-thread COM and the system endpoint class.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|_| Error::Unavailable)?;
        // SAFETY: select the local render endpoint once, never a capture source.
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .map_err(|_| Error::Unavailable)?;
        let id = endpoint_id(&device)?;
        // SAFETY: live render endpoint, initialized COM, no activation pointer.
        let client: IAudioClient =
            unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|_| Error::Unavailable)?;
        let format = WAVEFORMATEX {
            wFormatTag: 1,
            nChannels: u16::from(channels),
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * u32::from(channels) * 2,
            nBlockAlign: u16::from(channels) * 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        // SAFETY: complete PCM16 format borrowed only for this call. The shared
        // engine performs the single requested format/rate conversion boundary.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_100NS,
                0,
                &raw const format,
                None,
            )
        }
        .map_err(|_| Error::Unavailable)?;
        // SAFETY: initialized client; all returned values are owned scalars.
        let capacity = unsafe { client.GetBufferSize() }.map_err(|_| Error::Native)?;
        if capacity < largest || capacity > 1920 {
            return Err(Error::BufferLimit);
        }
        // SAFETY: initialized same-thread client; lifetime latency is immutable.
        let latency = unsafe { client.GetStreamLatency() }.map_err(|_| Error::Native)?;
        let (lead, idle_lead) = policy::leads(latency, largest)?;
        // SAFETY: both services belong to this initialized render stream.
        let render = unsafe { client.GetService() }.map_err(|_| Error::Unavailable)?;
        // SAFETY: as above; the speaker clock is independent of buffer credit.
        let clock: IAudioClock = unsafe { client.GetService() }.map_err(|_| Error::Unavailable)?;
        // SAFETY: live clock interface, scalar output only.
        let frequency = unsafe { clock.GetFrequency() }.map_err(|_| Error::Clock)?;
        if frequency == 0 {
            return Err(Error::Clock);
        }
        let qpc_frequency = qpc_frequency()?;
        let sleep = sleep_offset()?;
        let prime = policy::startup_prime(capacity, lead)?;
        let tail = Tail::new(prime)?;
        let inner = Interfaces {
            render,
            clock,
            client,
            device,
            enumerator,
            _com: com,
        };
        retirement.0.store(false, Ordering::Release);
        let mut output = Self {
            inner: Some(inner),
            id,
            channels,
            capacity,
            latency,
            frequency,
            qpc_frequency,
            sleep,
            lead,
            idle_lead,
            tail,
            pcm_started: false,
            closed: None,
            retirement,
            slot,
        };
        output.prime(prime)?;
        // SAFETY: one initialized, stopped stream with a bounded silent prefix.
        unsafe { output.inner.as_ref().ok_or(Error::Closed)?.client.Start() }
            .map_err(|_| Error::Unavailable)?;
        Ok(output)
    }

    pub const fn lead(&self) -> u64 {
        self.lead
    }
    pub fn capacity_bytes(&self) -> u32 {
        self.capacity * u32::from(self.channels) * 2
    }

    fn prime(&mut self, frames: u32) -> Result<(), Error> {
        let inner = self.inner.as_ref().ok_or(Error::Closed)?;
        // SAFETY: bounded frames fit this stopped stream's entire free buffer.
        let data = unsafe { inner.render.GetBuffer(frames) }.map_err(|_| Error::Native)?;
        let lease = Lease {
            render: &inner.render,
            frames,
            released: false,
        };
        if data.is_null() {
            return Err(Error::Native);
        }
        lease.release(true)
    }

    fn reading(&self) -> Result<Reading, Error> {
        let inner = self.inner.as_ref().ok_or(Error::Closed)?;
        self.sleep.unchanged(sleep_offset()?)?;
        // SAFETY: the originally pinned endpoint remains live in this owner.
        if unsafe { inner.device.GetState() }.map_err(|_| Error::Unavailable)?
            != DEVICE_STATE_ACTIVE
        {
            return Err(Error::DeviceChanged);
        }
        // SAFETY: compare the current default; never follow its replacement.
        let current = unsafe { inner.enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .map_err(|_| Error::Unavailable)?;
        if endpoint_id(&current)? != self.id {
            return Err(Error::DeviceChanged);
        }
        // SAFETY: initialized original client; its lifetime latency is immutable.
        let latency = unsafe { inner.client.GetStreamLatency() }.map_err(|_| Error::Unavailable)?;
        // SAFETY: exact original speaker-clock service, scalar output only.
        let frequency = unsafe { inner.clock.GetFrequency() }.map_err(|_| Error::Clock)?;
        if latency != self.latency || frequency != self.frequency {
            return Err(Error::DeviceChanged);
        }
        let (mut position, mut qpc) = (0, 0);
        // SAFETY: the exact live IAudioClock vtable and two writable scalars.
        // The generated .ok() wrapper merges S_FALSE with S_OK; use the original
        // HRESULT so an explicitly inaccurate clock never becomes evidence.
        let status = unsafe {
            (inner.clock.vtable().GetPosition)(
                inner.clock.as_raw(),
                &raw mut position,
                &raw mut qpc,
            )
        };
        if status.0 == 1 {
            return Err(Error::Pending);
        }
        if status.0 != 0 {
            return Err(Error::Unavailable);
        }
        // SAFETY: initialized original client, scalar allocation credit only.
        // Read padding after the clock: later native queued frames cannot exceed
        // the earlier written-to-speaker extent. Reversing this order can reject
        // valid motion between reads. The final QPC check also bounds this call.
        let padding =
            unsafe { inner.client.GetCurrentPadding() }.map_err(|_| Error::Unavailable)?;
        policy::fresh_clock(qpc, qpc_now(self.qpc_frequency)?)?;
        let samples = policy::samples(position, self.frequency)?;
        self.tail.observe(samples, padding, self.capacity)?;
        Ok(Reading { samples, padding })
    }

    pub fn poll(
        &mut self,
        mut permission: impl FnMut() -> Result<(), Error>,
    ) -> Result<Reading, Error> {
        let reading = self.reading()?;
        if !self.pcm_started
            && let Some(write) = self.tail.silence(reading.samples, self.idle_lead)?
        {
            return self.write(write, None, |_| permission());
        }
        Ok(reading)
    }

    pub fn submit(
        &mut self,
        pcm: &[i16],
        frames: u32,
        window: Window,
        permission: impl FnMut(Reading) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let (start, end) = window.scheduled()?;
        let write = self.tail.pcm(start, end, frames)?;
        self.write(write, Some(pcm), permission)?;
        self.pcm_started = true;
        Ok(())
    }

    fn write(
        &mut self,
        write: Write,
        pcm: Option<&[i16]>,
        mut permission: impl FnMut(Reading) -> Result<(), Error>,
    ) -> Result<Reading, Error> {
        let before = self.reading()?;
        if write.frames == 0 || write.frames > self.capacity - before.padding {
            return Err(Error::Backpressure);
        }
        let values = usize::try_from(write.frames)
            .ok()
            .and_then(|n| n.checked_mul(usize::from(self.channels)))
            .ok_or(Error::BufferLimit)?;
        let silent_values = usize::try_from(write.silence)
            .ok()
            .and_then(|n| n.checked_mul(usize::from(self.channels)))
            .ok_or(Error::BufferLimit)?;
        if silent_values > values
            || values > 1920 * 2
            || pcm.map_or(silent_values != values, |data| {
                data.len() != values - silent_values
            })
        {
            return Err(Error::Metadata);
        }
        let inner = self.inner.as_ref().ok_or(Error::Closed)?;
        // SAFETY: checked count fits current original native buffer credit.
        let data =
            unsafe { inner.render.GetBuffer(write.frames) }.map_err(|_| Error::Backpressure)?;
        let lease = Lease {
            render: &inner.render,
            frames: write.frames,
            released: false,
        };
        if data.is_null() {
            return Err(Error::Native);
        }
        if let Some(pcm) = pcm {
            // SAFETY: this live native lease owns values*2 writable bytes. The
            // silent prefix and interleaved PCM sizes were checked above. Windows
            // PCM16 is little-endian; byte copies impose no native alignment.
            unsafe {
                std::ptr::write_bytes(data, 0, silent_values * 2);
                std::ptr::copy_nonoverlapping(
                    pcm.as_ptr().cast::<u8>(),
                    data.add(silent_values * 2),
                    pcm.len() * 2,
                );
            }
        }
        // All driver preparation precedes the final original permission check.
        // ReleaseBuffer(0) abandons this exact lease on every refusal/unwind.
        let after = self.reading()?;
        permission(after)?;
        lease.release(pcm.is_none())?;
        self.tail.committed(write);
        Ok(after)
    }

    pub fn close(&mut self) -> Result<(), Error> {
        if let Some(result) = self.closed {
            return result;
        }
        let result = if let Some(inner) = self.inner.take() {
            // SAFETY: same-thread initialized client, no outstanding lease.
            let stopped = unsafe { inner.client.Stop() }.is_ok();
            // SAFETY: Reset is permitted only after confirmed stop and discards
            // this stream's queued sound rather than waiting for it to drain.
            let reset = stopped && unsafe { inner.client.Reset() }.is_ok();
            // Interface drop order is render, clock, client, endpoint, enumerator,
            // then matching COM uninitialization. No callback retains our state.
            drop(inner);
            if stopped && reset {
                Ok(())
            } else {
                Err(Error::Native)
            }
        } else {
            Ok(())
        };
        if result.is_ok() {
            self.retirement.0.store(true, Ordering::Release);
        } else {
            // Unknown native retirement consumes its original owner credit.
            // Four repeated failures cannot create an unbounded output family.
            self.slot.release = false;
        }
        self.closed = Some(result);
        result
    }
}
impl Drop for Render {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct Lease<'a> {
    render: &'a IAudioRenderClient,
    frames: u32,
    released: bool,
}
impl Lease<'_> {
    fn release(mut self, silent: bool) -> Result<(), Error> {
        self.released = true;
        // SAFETY: exact outstanding packet, same thread, all copied bytes are
        // initialized. 2 is AUDCLNT_BUFFERFLAGS_SILENT; failure is never retried.
        unsafe {
            self.render
                .ReleaseBuffer(self.frames, if silent { 2 } else { 0 })
        }
        .map_err(|_| Error::SubmissionUnknown)
    }
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if !self.released {
            // SAFETY: shared-mode ReleaseBuffer(0) refuses consumption and returns
            // the complete acquired packet; the enclosing owner is terminal.
            let _ = unsafe { self.render.ReleaseBuffer(0, 0) };
        }
    }
}

fn endpoint_id(device: &IMMDevice) -> Result<Vec<u16>, Error> {
    // SAFETY: the endpoint returns caller-owned, COM-allocated NUL-terminated UTF16.
    let text = unsafe { device.GetId() }.map_err(|_| Error::Unavailable)?;
    let result = (|| {
        if text.0.is_null() {
            return Err(Error::Unavailable);
        }
        let mut id = Vec::new();
        id.try_reserve_exact(MAX_ENDPOINT_UNITS)
            .map_err(|_| Error::Allocation)?;
        for index in 0..MAX_ENDPOINT_UNITS {
            // SAFETY: read only within the documented NUL-terminated native
            // string and stop at its terminator; impose an independent bound.
            let unit = unsafe { text.0.add(index).read() };
            if unit == 0 {
                return if id.is_empty() {
                    Err(Error::Unavailable)
                } else {
                    Ok(id)
                };
            }
            id.push(unit);
        }
        Err(Error::BufferLimit)
    })();
    // SAFETY: precisely the GetId allocation, freed once on all paths.
    unsafe { CoTaskMemFree(Some(text.0.cast())) };
    result
}

fn qpc_frequency() -> Result<u64, Error> {
    let mut frequency = 0;
    // SAFETY: writable scalar, no retained pointer.
    unsafe { QueryPerformanceFrequency(&raw mut frequency) }.map_err(|_| Error::Clock)?;
    u64::try_from(frequency)
        .ok()
        .filter(|n| *n != 0)
        .ok_or(Error::Clock)
}
fn qpc_now(frequency: u64) -> Result<u64, Error> {
    let mut counter = 0;
    // SAFETY: writable scalar, no retained pointer.
    unsafe { QueryPerformanceCounter(&raw mut counter) }.map_err(|_| Error::Clock)?;
    let counter = u64::try_from(counter).map_err(|_| Error::Clock)?;
    u64::try_from(u128::from(counter) * 10_000_000 / u128::from(frequency))
        .map_err(|_| Error::Clock)
}
fn sleep_offset() -> Result<SleepOffset, Error> {
    // SAFETY: three counter reads, no pointers retained. The outer reads include
    // sleep while the middle one excludes it; sampling width includes suspend.
    let (before, awake, after) = unsafe {
        (
            QueryInterruptTimePrecise(),
            QueryUnbiasedInterruptTimePrecise(),
            QueryInterruptTimePrecise(),
        )
    };
    SleepOffset::new(before, awake, after)
}
