//! The Windows backend: the system HEVC decoder (a synchronous Media
//! Foundation transform, e.g. Microsoft's HEVC Video Extension) given the
//! process's one D3D11 device, so DXVA decodes each picture into an NV12
//! D3D11 texture that the client's swap chain presents from the same device.
//! A picture never leaves GPU memory on that path.
//!
//! Input is Annex B: each admitted sample's 4-byte length prefixes become
//! start codes, and an IDR carries the record's VPS, SPS and PPS in front.
//! The transform runs in low-latency mode, so one admitted unit yields its
//! own picture; a transform that holds a picture back answers no picture
//! for that unit (counted, never invented).
use super::{Error, NV12_FULL_RANGE, NV12_VIDEO_RANGE};
use core::ffi::c_void;
use core::mem::ManuallyDrop;
use std::sync::OnceLock;
use windows::Win32::Foundation::{E_FAIL, HMODULE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaType, IMFSample,
    IMFTransform, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE,
    MF_VERSION, MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFSTARTUP_NOSOCKET, MFSampleExtension_CleanPoint, MFStartup,
    MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_HEVC,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::core::Interface;

/// The process's one D3D11 device, with the Media Foundation device manager
/// that hands it to decoders. Multithread-protected: the decode thread (via
/// the transform) and the window's present thread share its context.
pub struct Gpu {
    pub device: ID3D11Device,
    manager: IMFDXGIDeviceManager,
}
// SAFETY: D3D11 devices are free-threaded, the immediate context is
// serialized by `SetMultithreadProtected(true)` (set before publication), and
// the DXGI device manager is Media Foundation's thread-safe broker.
unsafe impl Send for Gpu {}
// SAFETY: as above; every use goes through thread-safe COM methods.
unsafe impl Sync for Gpu {}

/// The shared device, created once (hardware, video and BGRA support).
/// `None` when this machine has no D3D11 hardware device or Media Foundation.
pub fn gpu() -> Option<&'static Gpu> {
    static GPU: OnceLock<Option<Gpu>> = OnceLock::new();
    GPU.get_or_init(|| create_gpu().ok()).as_ref()
}

fn create_gpu() -> windows::core::Result<Gpu> {
    // SAFETY: plain Win32/COM calls with valid out pointers; every returned
    // interface is owned by its `windows` wrapper.
    unsafe {
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)?;
        let levels: [D3D_FEATURE_LEVEL; 2] = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut device = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&raw mut device),
            None,
            None,
        )?;
        let device = device.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        let _ = device
            .cast::<ID3D11Multithread>()?
            .SetMultithreadProtected(true);
        let mut token = 0_u32;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&raw mut token, &raw mut manager)?;
        let manager = manager.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        manager.ResetDevice(&device, token)?;
        Ok(Gpu { device, manager })
    }
}

/// One decoded picture: the transform's output sample (an NV12 D3D11
/// texture subresource on the DXVA path, system memory otherwise).
pub struct Image {
    sample: IMFSample,
    width: u32,
    height: u32,
    full_range: bool,
}
// SAFETY: Media Foundation samples and their buffers are free-threaded COM
// objects; this owner holds one reference and never writes the pixels.
unsafe impl Send for Image {}
impl Image {
    /// A new +1 `IMFSample` reference for a caller that releases it
    /// (`IMFSample::from_raw`).
    pub fn retained_raw(&self) -> *mut c_void {
        self.sample.clone().into_raw()
    }
    /// (visible width, visible height, NV12 range code, GPU texture).
    pub fn describe(&self) -> (u32, u32, u32, bool) {
        let format = if self.full_range {
            NV12_FULL_RANGE
        } else {
            NV12_VIDEO_RANGE
        };
        (self.width, self.height, format, self.texture().is_some())
    }
    /// The picture's D3D11 texture and array slice (the DXVA path).
    pub fn texture(&self) -> Option<(ID3D11Texture2D, u32)> {
        texture_of(&self.sample)
    }
    /// Mean of the visible luma plane (tests: a decoded picture, not a
    /// blank buffer).
    pub fn mean_luma(&self) -> Option<f64> {
        let rows = match self.texture() {
            Some((texture, slice)) => read_texture_luma(&texture, slice, self.width, self.height),
            None => read_memory_luma(&self.sample, self.width, self.height),
        }?;
        let count = f64::from(self.width) * f64::from(self.height);
        #[allow(clippy::cast_precision_loss, reason = "a luma sum is far below 2^52")]
        let sum = rows as f64;
        (count > 0.0).then(|| sum / count)
    }
}

/// A decoded sample's D3D11 texture and array slice (the DXVA path).
pub fn texture_of(sample: &IMFSample) -> Option<(ID3D11Texture2D, u32)> {
    // SAFETY: COM queries on a live sample; the out pointer receives a +1
    // texture reference that `from_raw` takes over.
    unsafe {
        let buffer = sample.GetBufferByIndex(0).ok()?;
        let dxgi = buffer.cast::<IMFDXGIBuffer>().ok()?;
        let mut raw = core::ptr::null_mut();
        dxgi.GetResource(&ID3D11Texture2D::IID, &raw mut raw).ok()?;
        if raw.is_null() {
            return None;
        }
        let texture = ID3D11Texture2D::from_raw(raw);
        Some((texture, dxgi.GetSubresourceIndex().ok()?))
    }
}

/// Sum of the visible luma bytes of one texture slice, via a staging copy.
fn read_texture_luma(
    texture: &ID3D11Texture2D,
    slice: u32,
    width: u32,
    height: u32,
) -> Option<u64> {
    let gpu = gpu()?;
    // SAFETY: a staging copy of one subresource on the shared device, mapped
    // for reading while the multithread-protected context is held by Map;
    // reads stay inside `height` rows of `RowPitch` bytes, `width` <= pitch.
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&raw mut desc);
        if desc.Width < width || desc.Height < height {
            return None;
        }
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        gpu.device
            .CreateTexture2D(&raw const desc, None, Some(&raw mut staging))
            .ok()?;
        let staging = staging?;
        let context = gpu.device.GetImmediateContext().ok()?;
        context.CopySubresourceRegion(&staging, 0, 0, 0, 0, texture, slice, None);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))
            .ok()?;
        let sum = sum_rows(
            mapped.pData.cast::<u8>(),
            mapped.RowPitch as usize,
            width,
            height,
        );
        context.Unmap(&staging, 0);
        Some(sum)
    }
}

/// Sum of the visible luma bytes of a system-memory NV12 sample.
fn read_memory_luma(sample: &IMFSample, width: u32, height: u32) -> Option<u64> {
    // SAFETY: Lock2D yields the plane base and its pitch for the locked
    // duration; reads stay inside `height` rows of `pitch` bytes.
    unsafe {
        let buffer = sample.GetBufferByIndex(0).ok()?;
        let planar = buffer.cast::<IMF2DBuffer>().ok()?;
        let mut base = core::ptr::null_mut();
        let mut pitch = 0_i32;
        planar.Lock2D(&raw mut base, &raw mut pitch).ok()?;
        let sum = usize::try_from(pitch)
            .ok()
            .filter(|pitch| *pitch >= width as usize)
            .map(|pitch| sum_rows(base, pitch, width, height));
        let _ = planar.Unlock2D();
        sum
    }
}

/// # Safety
/// `base` must address `height` readable rows of `pitch` bytes each.
unsafe fn sum_rows(base: *const u8, pitch: usize, width: u32, height: u32) -> u64 {
    let mut sum = 0_u64;
    for row in 0..height as usize {
        // SAFETY: the caller guarantees `height` rows of `pitch` >= `width`
        // bytes starting at `base`.
        let line = unsafe { core::slice::from_raw_parts(base.add(row * pitch), width as usize) };
        sum += line.iter().map(|&b| u64::from(b)).sum::<u64>();
    }
    sum
}

/// One HEVC decoder transform.
pub(super) struct RawDecoder {
    transform: IMFTransform,
    sets: Vec<u8>,
    width: u32,
    height: u32,
    full_range: bool,
    dxva: bool,
    provides_samples: bool,
    output_size: u32,
    annex_b: Vec<u8>,
}
// SAFETY: the transform is used by one thread at a time (`&mut self`); a
// synchronous Media Foundation transform has no thread affinity.
unsafe impl Send for RawDecoder {}

const START: [u8; 4] = [0, 0, 0, 1];

impl RawDecoder {
    pub(super) fn new(
        sets: [&[u8]; 3],
        width: u32,
        height: u32,
        full_range: bool,
    ) -> Result<Self, Error> {
        // This thread joins the process's multithreaded apartment (an
        // already-initialized thread keeps its own; both work for a
        // synchronous transform).
        // SAFETY: plain COM initialization for the calling thread.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let gpu = gpu().ok_or(Error::Unavailable)?;
        let transform = hevc_transform()?;
        let mut parameter_sets = Vec::new();
        for set in sets {
            parameter_sets.extend_from_slice(&START);
            parameter_sets.extend_from_slice(set);
        }
        let mut decoder = Self {
            transform,
            sets: parameter_sets,
            width,
            height,
            full_range,
            dxva: false,
            provides_samples: false,
            output_size: 0,
            annex_b: Vec::new(),
        };
        decoder.configure(gpu)?;
        Ok(decoder)
    }

    fn configure(&mut self, gpu: &Gpu) -> Result<(), Error> {
        // SAFETY: COM calls on a live transform and media types this
        // function owns; the device manager outlives the process.
        unsafe {
            let attributes = self
                .transform
                .GetAttributes()
                .map_err(|_| Error::Unavailable)?;
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0 {
                self.dxva = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, gpu.manager.as_raw() as usize)
                    .is_ok();
            }
            let input = MFCreateMediaType().map_err(|_| Error::Allocation)?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|_| Error::Allocation)?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)
                .map_err(|_| Error::Allocation)?;
            input
                .SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    (u64::from(self.width) << 32) | u64::from(self.height),
                )
                .map_err(|_| Error::Allocation)?;
            input
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|_| Error::Allocation)?;
            self.transform
                .SetInputType(0, &input, 0)
                .map_err(|_| Error::InvalidConfiguration)?;
            self.select_nv12()?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|_| Error::Codec)?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|_| Error::Codec)?;
        }
        Ok(())
    }

    /// Picks the transform's NV12 output (again after a stream change).
    fn select_nv12(&mut self) -> Result<(), Error> {
        // SAFETY: COM calls on a live transform; each enumerated type is
        // owned by its wrapper.
        unsafe {
            let mut index = 0;
            let chosen: IMFMediaType = loop {
                let Ok(candidate) = self.transform.GetOutputAvailableType(0, index) else {
                    return Err(Error::Unavailable);
                };
                if candidate.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    break candidate;
                }
                index += 1;
            };
            self.transform
                .SetOutputType(0, &chosen, 0)
                .map_err(|_| Error::Unavailable)?;
            let info = self
                .transform
                .GetOutputStreamInfo(0)
                .map_err(|_| Error::Codec)?;
            let provides = (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
            self.provides_samples = info.dwFlags & provides != 0;
            self.output_size = info.cbSize;
        }
        Ok(())
    }

    /// Whether DXVA decodes (the transform took the D3D11 device).
    #[allow(
        clippy::unnecessary_wraps,
        reason = "the Apple backend can answer unknown"
    )]
    pub(super) fn hardware(&self) -> Option<bool> {
        Some(self.dxva)
    }

    /// Decode one admitted sample: 4-byte length-prefixed NAL units without
    /// in-band parameter sets (`HevcGuard::prepare_hvc1`).
    pub(super) fn decode(
        &mut self,
        sample: &[u8],
        idr: bool,
        pts: i64,
        present: bool,
    ) -> Result<Option<Image>, Error> {
        self.annex_b.clear();
        if idr {
            self.annex_b.extend_from_slice(&self.sets);
        }
        let mut rest = sample;
        while !rest.is_empty() {
            let (prefix, tail) = rest.split_first_chunk::<4>().ok_or(Error::Codec)?;
            let length = u32::from_be_bytes(*prefix) as usize;
            let nal = tail.get(..length).ok_or(Error::Codec)?;
            self.annex_b.extend_from_slice(&START);
            self.annex_b.extend_from_slice(nal);
            rest = &tail[length..];
        }
        let input = self.input_sample(idr, pts)?;
        // SAFETY: a live transform and a sample this function owns.
        unsafe { self.transform.ProcessInput(0, &input, 0) }.map_err(|_| Error::Codec)?;
        let Some(output) = self.drain()? else {
            return Ok(None);
        };
        Ok(present.then_some(Image {
            sample: output,
            width: self.width,
            height: self.height,
            full_range: self.full_range,
        }))
    }

    fn input_sample(&self, idr: bool, pts: i64) -> Result<IMFSample, Error> {
        let length = u32::try_from(self.annex_b.len()).map_err(|_| Error::Codec)?;
        // SAFETY: the buffer is locked for exactly the copy of `length` bytes
        // into its `length`-byte allocation.
        unsafe {
            let buffer = MFCreateMemoryBuffer(length).map_err(|_| Error::Allocation)?;
            let mut base = core::ptr::null_mut();
            buffer
                .Lock(&raw mut base, None, None)
                .map_err(|_| Error::Allocation)?;
            core::ptr::copy_nonoverlapping(self.annex_b.as_ptr(), base, self.annex_b.len());
            buffer.Unlock().map_err(|_| Error::Allocation)?;
            buffer
                .SetCurrentLength(length)
                .map_err(|_| Error::Allocation)?;
            let sample = MFCreateSample().map_err(|_| Error::Allocation)?;
            sample.AddBuffer(&buffer).map_err(|_| Error::Allocation)?;
            // Capture microseconds to Media Foundation's 100 ns units.
            let _ = sample.SetSampleTime(pts.saturating_mul(10));
            if idr {
                let _ = sample.SetUINT32(&MFSampleExtension_CleanPoint, 1);
            }
            Ok(sample)
        }
    }

    /// The transform's next picture, if it has one now.
    fn drain(&mut self) -> Result<Option<IMFSample>, Error> {
        // A stream change renegotiates NV12 once and asks again; a second
        // change in one call is a codec failure.
        for _ in 0..2 {
            let provided = if self.provides_samples {
                None
            } else {
                // SAFETY: a fresh sample with one buffer of the size the
                // transform asked for.
                unsafe {
                    let sample = MFCreateSample().map_err(|_| Error::Allocation)?;
                    let buffer =
                        MFCreateMemoryBuffer(self.output_size).map_err(|_| Error::Allocation)?;
                    sample.AddBuffer(&buffer).map_err(|_| Error::Allocation)?;
                    Some(sample)
                }
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(provided),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0_u32;
            // SAFETY: one output buffer descriptor for stream 0; ownership of
            // its sample and events returns to this function below.
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, &mut buffers, &raw mut status)
            };
            let [buffer] = buffers;
            let sample = ManuallyDrop::into_inner(buffer.pSample);
            drop(ManuallyDrop::into_inner(buffer.pEvents));
            match result {
                Ok(()) => return Ok(sample),
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.select_nv12()?,
                Err(_) => return Err(Error::Codec),
            }
        }
        Err(Error::Codec)
    }
}

/// The first synchronous HEVC-to-NV12 decoder transform Media Foundation
/// offers (sorted by merit); `Unavailable` when none is installed (Windows
/// without the HEVC Video Extension).
fn hevc_transform() -> Result<IMFTransform, Error> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_HEVC,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let mut activates: *mut Option<IMFActivate> = core::ptr::null_mut();
    let mut count = 0_u32;
    // SAFETY: MFTEnumEx returns a CoTaskMem array of `count` activation
    // objects; each is taken (released by its wrapper) before the array is
    // freed.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&raw const input),
            Some(&raw const output),
            &raw mut activates,
            &raw mut count,
        )
        .map_err(|_| Error::Unavailable)?;
        if activates.is_null() {
            return Err(Error::Unavailable);
        }
        let all: Vec<IMFActivate> = (0..count as usize)
            .filter_map(|n| (*activates.add(n)).take())
            .collect();
        CoTaskMemFree(Some(activates.cast_const().cast()));
        all.iter()
            .find_map(|activate| activate.ActivateObject::<IMFTransform>().ok())
            .ok_or(Error::Unavailable)
    }
}
