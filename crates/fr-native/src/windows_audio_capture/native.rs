//! Narrow, thread-confined WASAPI/COM boundary. No callbacks retain Rust data.
use super::{Error, packing, policy};
use std::{marker::PhantomData, rc::Rc};
use windows::Win32::{
    Media::Audio::{
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
        AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE, IAudioCaptureClient,
        IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, eConsole,
        eRender,
    },
    System::{
        Com::{
            CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
            CoUninitialize,
        },
        Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
    },
};

const MAX_ENDPOINT_UNITS: usize = 1024;
const BUFFER_100NS: i64 = 400_000; // 40 ms; actual capacity is checked below.

pub(super) struct Clock {
    frequency: u64,
}

impl Clock {
    pub fn new() -> Result<Self, Error> {
        let mut frequency = 0;
        // SAFETY: writable scalar lives across the kernel call; no retained pointer.
        unsafe { QueryPerformanceFrequency(&raw mut frequency) }.map_err(|_| Error::Clock)?;
        let frequency = u64::try_from(frequency)
            .ok()
            .filter(|n| *n != 0)
            .ok_or(Error::Clock)?;
        Ok(Self { frequency })
    }

    pub fn now(&self) -> Result<u64, Error> {
        let mut counter = 0;
        // SAFETY: writable scalar lives across the kernel call; no retained pointer.
        unsafe { QueryPerformanceCounter(&raw mut counter) }.map_err(|_| Error::Clock)?;
        let counter = u64::try_from(counter).map_err(|_| Error::Clock)?;
        u64::try_from(u128::from(counter) * 1_000_000_000 / u128::from(self.frequency))
            .map_err(|_| Error::Clock)
    }
}

struct Com {
    _thread: PhantomData<Rc<()>>,
}

impl Com {
    fn new() -> Result<Self, Error> {
        // SAFETY: this role initializes COM on its original thread. S_OK and
        // S_FALSE each require one matching CoUninitialize; failures do not.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|_| Error::Native)?;
        Ok(Self {
            _thread: PhantomData,
        })
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: the guard is !Send and all COM objects drop before it.
        unsafe { CoUninitialize() };
    }
}

fn endpoint_id(device: &IMMDevice) -> Result<Vec<u16>, Error> {
    // SAFETY: a live COM endpoint; the returned null-terminated string is
    // allocated by COM and belongs to this caller until CoTaskMemFree.
    let text = unsafe { device.GetId() }.map_err(|_| Error::Unavailable)?;
    let result = (|| {
        if text.0.is_null() {
            return Err(Error::Unavailable);
        }
        let mut id = Vec::with_capacity(MAX_ENDPOINT_UNITS);
        for index in 0..MAX_ENDPOINT_UNITS {
            // SAFETY: GetId promises a valid NUL-terminated wide string. Read
            // only to its terminator and impose an independent local bound.
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
    // SAFETY: precisely the GetId allocation, freed once on every result.
    unsafe { CoTaskMemFree(Some(text.0.cast())) };
    result
}

pub(super) struct Packet {
    pub position: u64,
    pub qpc_100ns: u64,
    pub frames: u32,
    pub flags: u32,
}

pub(super) struct Capture {
    // Drop order is deliberate: service before client, then endpoints, then COM.
    reader: IAudioCaptureClient,
    client: IAudioClient,
    device: IMMDevice,
    enumerator: IMMDeviceEnumerator,
    id: Vec<u16>,
    channels: u8,
    buffer_frames: u32,
    started: bool,
    _com: Com,
}

impl Capture {
    pub fn open(channels: u8) -> Result<Self, Error> {
        if !matches!(channels, 1 | 2) {
            return Err(Error::Configuration);
        }
        let com = Com::new()?;
        // SAFETY: initialized COM and the documented system enumerator class.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|_| Error::Unavailable)?;
        // SAFETY: render endpoint only, resolved once; no microphone source.
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .map_err(|_| Error::Unavailable)?;
        let id = endpoint_id(&device)?;
        // SAFETY: live render endpoint with initialized COM, no activation data.
        let client: IAudioClient =
            unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|_| Error::Unavailable)?;
        let format = WAVEFORMATEX {
            wFormatTag: 1, // WAVE_FORMAT_PCM, signed 16-bit little endian.
            nChannels: u16::from(channels),
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * u32::from(channels) * 2,
            nBlockAlign: u16::from(channels) * 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        // SAFETY: a complete, correctly sized PCM format, retained only for
        // this synchronous call. The engine converts its mix to this explicit
        // stream format; an unsupported format/loopback combination refuses.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_100NS,
                0,
                &raw const format,
                None,
            )
        }
        .map_err(|_| Error::Format)?;
        // SAFETY: initialized client; scalar result has no borrowed storage.
        let buffer_frames = unsafe { client.GetBufferSize() }.map_err(|_| Error::Native)?;
        if buffer_frames == 0
            || usize::try_from(buffer_frames).map_or(true, |n| n > packing::MAX_CHUNK_SAMPLES)
        {
            return Err(Error::BufferLimit);
        }
        // SAFETY: the shared render endpoint was initialized with LOOPBACK.
        let reader = unsafe { client.GetService() }.map_err(|_| Error::Unavailable)?;
        let mut source = Self {
            reader,
            client,
            device,
            enumerator,
            id,
            channels,
            buffer_frames,
            started: false,
            _com: com,
        };
        source.check()?;
        // SAFETY: initialized, stopped stream with an owned capture service.
        unsafe { source.client.Start() }.map_err(|_| Error::Unavailable)?;
        source.started = true;
        Ok(source)
    }

    pub fn check(&self) -> Result<(), Error> {
        // SAFETY: the exact pinned endpoint remains alive in this owner.
        if unsafe { self.device.GetState() }.map_err(|_| Error::Unavailable)? != DEVICE_STATE_ACTIVE
        {
            return Err(Error::Unavailable);
        }
        // SAFETY: a live enumerator. A replacement is checked, never captured.
        let current = unsafe { self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .map_err(|_| Error::Unavailable)?;
        if endpoint_id(&current)? != self.id {
            return Err(Error::Unavailable);
        }
        // SAFETY: an initialized client; this also surfaces device/service or
        // stream invalidation before any retained frame can be published.
        let padding = unsafe { self.client.GetCurrentPadding() }.map_err(|_| Error::Unavailable)?;
        if padding > self.buffer_frames {
            return Err(Error::BufferLimit);
        }
        Ok(())
    }

    pub fn read(&self, output: &mut [i16]) -> Result<Option<Packet>, Error> {
        // SAFETY: all capture calls use this original COM thread.
        let next = unsafe { self.reader.GetNextPacketSize() }.map_err(|_| Error::Unavailable)?;
        if next == 0 {
            return Ok(None);
        }
        if next > self.buffer_frames {
            return Err(Error::BufferLimit);
        }
        let (mut bytes, mut frames, mut flags, mut position, mut qpc_100ns) =
            (std::ptr::null_mut(), 0, 0, 0, 0);
        // SAFETY: five writable output values. The buffer lease is handled
        // below on this same thread before any subsequent GetBuffer call.
        unsafe {
            self.reader.GetBuffer(
                &raw mut bytes,
                &raw mut frames,
                &raw mut flags,
                Some(&raw mut position),
                Some(&raw mut qpc_100ns),
            )
        }
        .map_err(|_| Error::Unavailable)?;
        if frames == 0 {
            return Ok(None);
        } // AUDCLNT_S_BUFFER_EMPTY has no lease.
        let lease = PacketLease {
            capture: &self.reader,
            frames,
            released: false,
        };
        if frames > self.buffer_frames
            || flags & !(policy::DISCONTINUITY | policy::SILENT | policy::TIMESTAMP_ERROR) != 0
        {
            return Err(Error::Format);
        }
        if flags & policy::TIMESTAMP_ERROR != 0 {
            return Err(Error::Clock);
        }
        let values = usize::try_from(frames)
            .ok()
            .and_then(|n| n.checked_mul(usize::from(self.channels)))
            .filter(|n| *n <= output.len() && *n <= packing::MAX_CHUNK_VALUES)
            .ok_or(Error::BufferLimit)?;
        let silent = flags & policy::SILENT != 0;
        let data = if silent || bytes.is_null() {
            None
        } else {
            // SAFETY: GetBuffer's live lease holds exactly the returned frames
            // of the accepted PCM16 stream. Count and capacity are checked;
            // use bytes so no alignment assumption crosses this boundary.
            Some(unsafe { std::slice::from_raw_parts(bytes, values * 2) })
        };
        policy::copy_pcm(data, values, silent, output)?;
        lease.release()?;
        Ok(Some(Packet {
            position,
            qpc_100ns,
            frames,
            flags,
        }))
    }

    pub fn close(mut self) -> Result<(), Error> {
        self.started = false;
        // SAFETY: the initialized source is stopped once; all interfaces drop
        // before the same-thread COM guard even if Stop returns an error.
        unsafe { self.client.Stop() }.map_err(|_| Error::Cleanup)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if self.started {
            self.started = false;
            // SAFETY: error-path cleanup for the original live stream. No
            // successful Stopped response is inferred from this best effort.
            let _ = unsafe { self.client.Stop() };
        }
    }
}

struct PacketLease<'a> {
    capture: &'a IAudioCaptureClient,
    frames: u32,
    released: bool,
}

impl PacketLease<'_> {
    fn release(mut self) -> Result<(), Error> {
        // A failed native release is uncertain; never retry it in Drop.
        self.released = true;
        // SAFETY: the exact complete packet from this outstanding GetBuffer,
        // released on the same thread, with no remaining borrowed PCM slices.
        unsafe { self.capture.ReleaseBuffer(self.frames) }.map_err(|_| Error::Cleanup)
    }
}

impl Drop for PacketLease<'_> {
    fn drop(&mut self) {
        if !self.released {
            // SAFETY: 0 is the documented refusal to consume the outstanding
            // packet. The caller is already terminal and will close the stream.
            let _ = unsafe { self.capture.ReleaseBuffer(0) };
        }
    }
}
