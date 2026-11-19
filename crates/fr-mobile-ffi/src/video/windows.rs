//! The Windows backend: the system HEVC decoder (a synchronous Media
//! Foundation transform, e.g. Microsoft's HEVC Video Extension) given the
//! process's one D3D11 device, so DXVA decodes each picture into an NV12
//! D3D11 texture that the client's swap chain presents from the same device.
//! A picture never leaves GPU memory on that path.
//!
//! When the decoder has no usable DXVA path (no GPU HEVC profile, a
//! transform that refuses the device, or an explicit software preference),
//! it decodes on the CPU into system memory. Each such picture is copied once
//! into a bounded ring of NV12 textures on the same device, so presentation
//! stays one GPU video-processor step on either path.
//!
//! Input is Annex B: each admitted sample's 4-byte length prefixes become
//! start codes, and an IDR carries the record's VPS, SPS and PPS in front.
//! One admitted unit must synchronously yield its own timestamp-matched
//! picture. Missing or delayed output fails the decoder transaction: it
//! cannot advance the reference chain or become a later input's picture.
use super::{Error, NV12_FULL_RANGE, NV12_VIDEO_RANGE};
use crate::windows_video_contract::{
    Allocation, MAX_OUTPUT_TYPES, MAX_TRANSFORMS, OutputBounds, owns_picture, readable_luma,
};
use core::ffi::c_void;
use core::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};
use windows::Win32::Foundation::{E_FAIL, HMODULE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_DECODER, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, D3D11_MAP_READ, D3D11_MAP_WRITE, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11Multithread, ID3D11Texture2D, ID3D11VideoDevice,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMF2DBuffer2, IMFActivate, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaType,
    IMFSample, IMFTransform, MF_E_ATTRIBUTENOTFOUND, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE, MF_VERSION,
    MF2DBuffer_LockFlags_Read, MFCreateAlignedMemoryBuffer, MFCreateDXGIDeviceManager,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFSTARTUP_NOSOCKET, MFSampleExtension_CleanPoint, MFStartup,
    MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_HEVC,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::core::Interface;

/// Which Media Foundation decode path this process's viewers may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecoderPreference {
    /// DXVA when the system decoder accepts the shared D3D11 device;
    /// otherwise its CPU path, uploaded once per picture to the same GPU.
    Automatic,
    /// Refuse a decoder (`Unavailable`) that does not accept the device.
    Hardware,
    /// Never hand the decoder the device: CPU decode, GPU presentation.
    Software,
}

static PREFERENCE: AtomicU8 = AtomicU8::new(0);

/// Set once at startup, before any session creates a decoder.
pub fn set_decoder_preference(preference: DecoderPreference) {
    let value = match preference {
        DecoderPreference::Automatic => 0,
        DecoderPreference::Hardware => 1,
        DecoderPreference::Software => 2,
    };
    PREFERENCE.store(value, Ordering::Relaxed);
}

pub fn decoder_preference() -> DecoderPreference {
    match PREFERENCE.load(Ordering::Relaxed) {
        1 => DecoderPreference::Hardware,
        2 => DecoderPreference::Software,
        _ => DecoderPreference::Automatic,
    }
}

/// Local decode readiness for setup guidance, without opening a session.
/// `gpu_hevc_profile` is the adapter's advertised HEVC Main decode profile,
/// not proof of a working decode: a session's `dxva` result is that proof.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DecoderSupport {
    /// A Media Foundation HEVC-to-NV12 decoder is registered (on most PCs
    /// Microsoft's HEVC Video Extension). Without one nothing can be viewed.
    pub decoder_installed: bool,
    /// The shared D3D11 device could be created.
    pub gpu_device: bool,
    /// The adapter advertises the D3D11 HEVC Main (VLD) decode profile.
    pub gpu_hevc_profile: bool,
    /// The adapter's description (for example "Intel(R) Iris(R) Plus
    /// Graphics"); bounded to 128 UTF-16 units by DXGI.
    pub adapter: Option<String>,
}

/// One bounded local probe: decoder registration plus the shared device's
/// adapter and decode profiles. No decode is run.
pub fn decoder_support() -> DecoderSupport {
    // SAFETY: plain COM initialization for the calling thread.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let mut support = DecoderSupport::default();
    let Some(gpu) = gpu() else {
        return support;
    };
    support.gpu_device = true;
    support.decoder_installed = hevc_transform().is_ok();
    // SAFETY: read-only COM queries on the live shared device; the adapter
    // description is a fixed-size UTF-16 array owned by the returned struct.
    unsafe {
        if let Ok(adapter) = gpu
            .device
            .cast::<IDXGIDevice>()
            .and_then(|dxgi| dxgi.GetAdapter())
            && let Ok(desc) = adapter.GetDesc()
        {
            let end = desc
                .Description
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..end]);
            support.adapter = (!name.trim().is_empty()).then(|| name.trim().to_owned());
        }
        if let Ok(video) = gpu.device.cast::<ID3D11VideoDevice>() {
            let count = video.GetVideoDecoderProfileCount().min(256);
            support.gpu_hevc_profile = (0..count).any(|index| {
                video
                    .GetVideoDecoderProfile(index)
                    .is_ok_and(|profile| profile == D3D11_DECODER_PROFILE_HEVC_VLD_MAIN)
            });
        }
    }
    support
}

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
        let sum = (!mapped.pData.is_null() && mapped.RowPitch >= width).then(|| {
            sum_rows(
                mapped.pData.cast::<u8>(),
                mapped.RowPitch as usize,
                width,
                height,
            )
        });
        context.Unmap(&staging, 0);
        sum
    }
}

/// Sum of the visible luma bytes of a system-memory NV12 sample.
fn read_memory_luma(sample: &IMFSample, width: u32, height: u32) -> Option<u64> {
    // SAFETY: Lock2D yields the plane base and its pitch for the locked
    // duration; reads stay inside `height` rows of `pitch` bytes.
    unsafe {
        let buffer = sample.GetBufferByIndex(0).ok()?;
        let planar = buffer.cast::<IMF2DBuffer>().ok()?;
        let capacity = buffer.GetMaxLength().ok()?;
        let mut base = core::ptr::null_mut();
        let mut pitch = 0_i32;
        planar.Lock2D(&raw mut base, &raw mut pitch).ok()?;
        let sum = u32::try_from(pitch)
            .ok()
            .filter(|pitch| !base.is_null() && readable_luma(width, height, *pitch, capacity))
            .map(|pitch| sum_rows(base, pitch as usize, width, height));
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
    coded: (u32, u32),
    bounds: OutputBounds,
    minimum_output_bytes: u32,
    full_range: bool,
    dxva: bool,
    annex_b: Vec<u8>,
    /// Luma rows in a system-memory output buffer (the selected NV12 type's
    /// frame height); the chroma plane starts this many pitches in.
    output_rows: u32,
    /// Bytes per row of a contiguous (non-2D) system-memory output buffer.
    output_pitch: u32,
    /// The CPU path's upload ring, created on its first picture.
    upload: Option<Upload>,
}

/// Textures in the CPU path's upload ring. A picture's slice is rewritten
/// only after this many newer pictures, long after the viewer has replaced
/// it; the frame slot itself retains just the newest picture.
const UPLOAD_SLICES: u32 = 4;

/// One CPU-writable NV12 staging texture and the GPU texture array the
/// video processor reads, both at the even-rounded visible size.
struct Upload {
    staging: ID3D11Texture2D,
    ring: ID3D11Texture2D,
    size: (u32, u32),
    next: u32,
}

impl Upload {
    fn new(device: &ID3D11Device, size: (u32, u32)) -> Result<Self, Error> {
        let desc = |array: u32, staging: bool, bind: u32| D3D11_TEXTURE2D_DESC {
            Width: size.0,
            Height: size.1,
            MipLevels: 1,
            ArraySize: array,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: if staging {
                D3D11_USAGE_STAGING
            } else {
                D3D11_USAGE_DEFAULT
            },
            BindFlags: bind,
            CPUAccessFlags: if staging {
                D3D11_CPU_ACCESS_WRITE.0 as u32
            } else {
                0
            },
            MiscFlags: 0,
        };
        let staging_desc = desc(1, true, 0);
        let (mut staging, mut ring) = (None, None);
        // SAFETY: texture creation on the live shared device with fully
        // initialized descriptions and owned out pointers.
        unsafe {
            device
                .CreateTexture2D(&raw const staging_desc, None, Some(&raw mut staging))
                .map_err(|_| Error::Allocation)?;
            // Drivers refuse an NV12 array without a bind flag. The decoder
            // flag matches DXVA's own output (the renderer's proven input);
            // a shader resource is the fallback.
            for bind in [D3D11_BIND_DECODER.0, D3D11_BIND_SHADER_RESOURCE.0] {
                let ring_desc = desc(UPLOAD_SLICES, false, bind.cast_unsigned());
                if device
                    .CreateTexture2D(&raw const ring_desc, None, Some(&raw mut ring))
                    .is_ok()
                {
                    break;
                }
            }
        }
        Ok(Self {
            staging: staging.ok_or(Error::Allocation)?,
            ring: ring.ok_or(Error::Allocation)?,
            size,
            next: 0,
        })
    }
}
// SAFETY: the transform is used by one thread at a time (`&mut self`); a
// synchronous Media Foundation transform has no thread affinity.
unsafe impl Send for RawDecoder {}

const START: [u8; 4] = [0, 0, 0, 1];
/// An HEVC access unit delimiter (NAL type 35, any picture type).
const NEXT_ACCESS_UNIT: [u8; 7] = [0, 0, 0, 1, 0x46, 0x01, 0x50];

impl RawDecoder {
    pub(super) fn new(
        sets: [&[u8]; 3],
        coded: (u32, u32),
        visible: (u32, u32),
        full_range: bool,
    ) -> Result<Self, Error> {
        let bounds = OutputBounds::new(coded, visible).ok_or(Error::Geometry)?;
        // This thread joins the process's multithreaded apartment (an
        // already-initialized thread keeps its own; both work for a
        // synchronous transform).
        // SAFETY: plain COM initialization for the calling thread.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let gpu = gpu().ok_or(Error::Unavailable)?;
        let transform = hevc_transform()?;
        let sets_length = sets
            .iter()
            .try_fold(0_usize, |length, set| {
                length.checked_add(START.len())?.checked_add(set.len())
            })
            .ok_or(Error::Allocation)?;
        let mut parameter_sets = Vec::new();
        parameter_sets
            .try_reserve_exact(sets_length)
            .map_err(|_| Error::Allocation)?;
        for set in sets {
            parameter_sets.extend_from_slice(&START);
            parameter_sets.extend_from_slice(set);
        }
        let mut decoder = Self {
            transform,
            sets: parameter_sets,
            width: visible.0,
            height: visible.1,
            coded,
            bounds,
            minimum_output_bytes: 0,
            full_range,
            dxva: false,
            annex_b: Vec::new(),
            output_rows: 0,
            output_pitch: 0,
            upload: None,
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
            let preference = decoder_preference();
            if preference != DecoderPreference::Software
                && attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0
            {
                self.dxva = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, gpu.manager.as_raw() as usize)
                    .is_ok();
            }
            if preference == DecoderPreference::Hardware && !self.dxva {
                return Err(Error::Unavailable);
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
                    (u64::from(self.coded.0) << 32) | u64::from(self.coded.1),
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
            for index in 0..MAX_OUTPUT_TYPES {
                let Ok(candidate) = self.transform.GetOutputAvailableType(0, index) else {
                    return Err(Error::Unavailable);
                };
                if candidate.GetGUID(&MF_MT_SUBTYPE).ok() != Some(MFVideoFormat_NV12) {
                    continue;
                }
                // MF can offer a partial type before the first input. Fill
                // only the absent geometry with our already admitted size;
                // an explicitly different size is still refused.
                if let Err(error) = candidate.GetUINT64(&MF_MT_FRAME_SIZE) {
                    if error.code() != MF_E_ATTRIBUTENOTFOUND {
                        return Err(Error::Geometry);
                    }
                    candidate
                        .SetUINT64(
                            &MF_MT_FRAME_SIZE,
                            (u64::from(self.coded.0) << 32) | u64::from(self.coded.1),
                        )
                        .map_err(|_| Error::Geometry)?;
                }
                self.output_layout(&candidate)?;
                self.transform
                    .SetOutputType(0, &candidate, 0)
                    .map_err(|_| Error::Unavailable)?;
                let selected = self
                    .transform
                    .GetOutputCurrentType(0)
                    .map_err(|_| Error::Codec)?;
                self.minimum_output_bytes = self.output_layout(&selected)?;
                let size = selected
                    .GetUINT64(&MF_MT_FRAME_SIZE)
                    .map_err(|_| Error::Geometry)?;
                self.output_rows =
                    u32::try_from(size & u64::from(u32::MAX)).map_err(|_| Error::Geometry)?;
                let width = u32::try_from(size >> 32).map_err(|_| Error::Geometry)?;
                self.output_pitch = selected.GetUINT32(&MF_MT_DEFAULT_STRIDE).unwrap_or(width);
                self.output_allocation()?;
                return Ok(());
            }
        }
        Err(Error::Unavailable)
    }

    fn output_layout(&self, media_type: &IMFMediaType) -> Result<u32, Error> {
        // SAFETY: read-only queries on a live media type. Missing stride is
        // allowed; malformed attributes and geometry are not.
        unsafe {
            if media_type.GetGUID(&MF_MT_SUBTYPE).ok() != Some(MFVideoFormat_NV12) {
                return Err(Error::Geometry);
            }
            let dimensions = media_type
                .GetUINT64(&MF_MT_FRAME_SIZE)
                .map_err(|_| Error::Geometry)?;
            let width = u32::try_from(dimensions >> 32).map_err(|_| Error::Geometry)?;
            let height =
                u32::try_from(dimensions & u64::from(u32::MAX)).map_err(|_| Error::Geometry)?;
            let stride = match media_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) {
                Ok(stride) => Some(stride),
                Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => None,
                Err(_) => return Err(Error::Geometry),
            };
            self.bounds
                .layout(width, height, stride)
                .ok_or(Error::Geometry)
        }
    }

    /// Stream info can change after `ProcessOutput`, even without a new type.
    /// Recheck it before every native allocation instead of caching `cbSize`.
    fn output_allocation(&self) -> Result<Allocation, Error> {
        // SAFETY: a live transform with its input and output types set.
        let info = unsafe { self.transform.GetOutputStreamInfo(0) }.map_err(|_| Error::Codec)?;
        let provides =
            (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
        self.bounds
            .allocation(
                self.minimum_output_bytes,
                info.cbSize,
                info.cbAlignment,
                info.dwFlags & provides != 0,
            )
            .ok_or(Error::Geometry)
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
        frame_time: i64,
        present: bool,
    ) -> Result<Option<Image>, Error> {
        self.annex_b.clear();
        let size = sample
            .len()
            .checked_add(if idr { self.sets.len() } else { 0 })
            .and_then(|size| size.checked_add(NEXT_ACCESS_UNIT.len()))
            .ok_or(Error::Allocation)?;
        self.annex_b
            .try_reserve_exact(size)
            .map_err(|_| Error::Allocation)?;
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
        if !self.dxva {
            // The software parser completes a picture only when the next
            // access unit starts; a delimiter starts it now.
            self.annex_b.extend_from_slice(&NEXT_ACCESS_UNIT);
        }
        let input = self.input_sample(idr, frame_time)?;
        // SAFETY: a live transform and a sample this function owns.
        unsafe { self.transform.ProcessInput(0, &input, 0) }.map_err(|_| Error::Codec)?;
        let mut output = self.drain(frame_time)?;
        if present && texture_of(&output).is_none() {
            output = self.upload(&output, frame_time)?;
        }
        Ok(present.then_some(Image {
            sample: output,
            width: self.width,
            height: self.height,
            full_range: self.full_range,
        }))
    }

    /// The CPU path: copy one validated system-memory NV12 picture into the
    /// next ring slice and return a texture sample carrying the original
    /// frame time, so presentation is the same GPU step as DXVA output.
    fn upload(&mut self, sample: &IMFSample, frame_time: i64) -> Result<IMFSample, Error> {
        let gpu = gpu().ok_or(Error::Unavailable)?;
        let even = |value: u32| value.checked_add(1).map(|value| value & !1);
        let size = (
            even(self.width).ok_or(Error::Geometry)?,
            even(self.height).ok_or(Error::Geometry)?,
        );
        if size.0 == 0 || size.1 == 0 || size.1 > self.output_rows {
            return Err(Error::Geometry);
        }
        if self
            .upload
            .as_ref()
            .is_none_or(|upload| upload.size != size)
        {
            self.upload = Some(Upload::new(&gpu.device, size)?);
        }
        let upload = self.upload.as_mut().ok_or(Error::Allocation)?;
        let slice = upload.next;
        upload.next = (upload.next + 1) % UPLOAD_SLICES;
        let rows = self.output_rows;
        // SAFETY: the source buffer is locked for reading for the whole copy
        // and unlocked on every path; every row read is bounds-checked
        // against the locked length below. The staging map is written only
        // inside its mapped `RowPitch` x (height * 3 / 2) area and unmapped
        // before the GPU copy. All objects belong to the shared device.
        unsafe {
            let buffer = sample.GetBufferByIndex(0).map_err(|_| Error::Codec)?;
            if let Ok(planar) = buffer.cast::<IMF2DBuffer2>() {
                // The decoder's own samples: its pitch and locked range.
                let mut scanline = core::ptr::null_mut();
                let mut pitch = 0_i32;
                let mut start = core::ptr::null_mut();
                let mut length = 0_u32;
                planar
                    .Lock2DSize(
                        MF2DBuffer_LockFlags_Read,
                        &raw mut scanline,
                        &raw mut pitch,
                        &raw mut start,
                        &raw mut length,
                    )
                    .map_err(|_| Error::Codec)?;
                let copied = copy_to_staging(
                    &gpu.device,
                    &upload.staging,
                    Source {
                        scanline: scanline.cast_const(),
                        start: start.cast_const(),
                        length,
                        pitch,
                        rows,
                    },
                    size,
                );
                let _ = planar.Unlock2D();
                copied?;
            } else {
                // A caller-allocated contiguous buffer laid out with the
                // negotiated output type's stride.
                let pitch = i32::try_from(self.output_pitch).map_err(|_| Error::Geometry)?;
                let mut base = core::ptr::null_mut();
                let mut length = 0_u32;
                buffer
                    .Lock(&raw mut base, None, Some(&raw mut length))
                    .map_err(|_| Error::Codec)?;
                let copied = copy_to_staging(
                    &gpu.device,
                    &upload.staging,
                    Source {
                        scanline: base.cast_const(),
                        start: base.cast_const(),
                        length,
                        pitch,
                        rows,
                    },
                    size,
                );
                let _ = buffer.Unlock();
                copied?;
            }
            let context = gpu.device.GetImmediateContext().map_err(|_| Error::Codec)?;
            context.CopySubresourceRegion(&upload.ring, slice, 0, 0, 0, &upload.staging, 0, None);
            let media =
                MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &upload.ring, slice, false)
                    .map_err(|_| Error::Allocation)?;
            let output = MFCreateSample().map_err(|_| Error::Allocation)?;
            output.AddBuffer(&media).map_err(|_| Error::Allocation)?;
            output.SetSampleTime(frame_time).map_err(|_| Error::Codec)?;
            Ok(output)
        }
    }

    fn input_sample(&self, idr: bool, frame_time: i64) -> Result<IMFSample, Error> {
        let length = u32::try_from(self.annex_b.len()).map_err(|_| Error::Codec)?;
        // SAFETY: the buffer is locked for exactly the copy of `length` bytes
        // into its `length`-byte allocation.
        unsafe {
            let buffer = MFCreateMemoryBuffer(length).map_err(|_| Error::Allocation)?;
            let mut base = core::ptr::null_mut();
            let mut capacity = 0;
            buffer
                .Lock(&raw mut base, Some(&raw mut capacity), None)
                .map_err(|_| Error::Allocation)?;
            if base.is_null() || capacity < length {
                let _ = buffer.Unlock();
                return Err(Error::Allocation);
            }
            core::ptr::copy_nonoverlapping(self.annex_b.as_ptr(), base, self.annex_b.len());
            buffer.Unlock().map_err(|_| Error::Allocation)?;
            buffer
                .SetCurrentLength(length)
                .map_err(|_| Error::Allocation)?;
            let sample = MFCreateSample().map_err(|_| Error::Allocation)?;
            sample.AddBuffer(&buffer).map_err(|_| Error::Allocation)?;
            // This token identifies the original access unit, not wall time.
            sample.SetSampleTime(frame_time).map_err(|_| Error::Codec)?;
            if idr {
                sample
                    .SetUINT32(&MFSampleExtension_CleanPoint, 1)
                    .map_err(|_| Error::Codec)?;
            }
            Ok(sample)
        }
    }

    /// The current input's one picture; missing output is a failed decode,
    /// never permission to carry a hidden picture into the next transaction.
    fn drain(&mut self, frame_time: i64) -> Result<IMFSample, Error> {
        // A stream change renegotiates NV12 once and asks again; a second
        // change in one call is a codec failure.
        for _ in 0..2 {
            let output = self.output_once()?;
            match output.result {
                Ok(()) => {
                    let sample = output.sample.ok_or(Error::Codec)?;
                    // SAFETY: this function owns the returned sample.
                    let time = unsafe { sample.GetSampleTime() }.ok();
                    if !owns_picture(
                        frame_time,
                        time,
                        output.stream_status,
                        output.status,
                        output.has_events,
                    ) {
                        return Err(Error::Codec);
                    }
                    self.validate_output(&sample)?;
                    return Ok(sample);
                }
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    return Err(Error::Codec);
                }
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.select_nv12()?,
                Err(_) => return Err(Error::Codec),
            }
        }
        Err(Error::Codec)
    }

    /// One `ProcessOutput` call with a sample of the admitted size when the
    /// transform asks the caller to allocate.
    fn output_once(&mut self) -> Result<Output, Error> {
        let provided = match self.output_allocation()? {
            Allocation::Decoder => None,
            Allocation::Caller {
                bytes,
                alignment_mask,
            } => {
                // SAFETY: a fresh sample with one buffer whose size and
                // alignment were checked against this admitted geometry.
                unsafe {
                    let sample = MFCreateSample().map_err(|_| Error::Allocation)?;
                    let buffer = MFCreateAlignedMemoryBuffer(bytes, alignment_mask)
                        .map_err(|_| Error::Allocation)?;
                    sample.AddBuffer(&buffer).map_err(|_| Error::Allocation)?;
                    Some(sample)
                }
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
        let events = ManuallyDrop::into_inner(buffer.pEvents);
        // SAFETY: the returned collection is owned until it is dropped.
        let has_events = events
            .as_ref()
            .is_some_and(|events| unsafe { events.GetElementCount() != Ok(0) });
        drop(events);
        Ok(Output {
            result,
            sample,
            stream_status: buffer.dwStatus,
            status,
            has_events,
        })
    }

    fn validate_output(&self, sample: &IMFSample) -> Result<(), Error> {
        // SAFETY: read-only COM queries on the live sample and its one buffer.
        // No returned pixels are mapped or handed to a renderer before these
        // geometry, allocation, and subresource checks succeed.
        unsafe {
            if sample.GetBufferCount().map_err(|_| Error::Codec)? != 1 {
                return Err(Error::Codec);
            }
            if let Some((texture, slice)) = texture_of(sample) {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&raw mut desc);
                if desc.Format != DXGI_FORMAT_NV12
                    || desc.MipLevels != 1
                    || desc.SampleDesc.Count != 1
                    || !self
                        .bounds
                        .texture(desc.Width, desc.Height, desc.ArraySize, slice)
                {
                    return Err(Error::Geometry);
                }
            } else {
                let buffer = sample.GetBufferByIndex(0).map_err(|_| Error::Codec)?;
                let capacity = buffer.GetMaxLength().map_err(|_| Error::Geometry)?;
                let length = buffer.GetCurrentLength().map_err(|_| Error::Geometry)?;
                if !self
                    .bounds
                    .memory(self.minimum_output_bytes, length, capacity)
                {
                    return Err(Error::Geometry);
                }
            }
        }
        Ok(())
    }
}

/// One `ProcessOutput` result with the sample and status it returned.
struct Output {
    result: windows::core::Result<()>,
    sample: Option<IMFSample>,
    stream_status: u32,
    status: u32,
    has_events: bool,
}

/// A locked system-memory NV12 picture: `rows` luma rows of `pitch` bytes
/// from `scanline`, then `rows / 2` interleaved chroma rows.
#[derive(Clone, Copy)]
struct Source {
    scanline: *const u8,
    start: *const u8,
    length: u32,
    pitch: i32,
    rows: u32,
}

/// Copies the visible `size` (even) of `source` into the mapped staging
/// texture. Every source row is checked against the locked byte range first.
///
/// # Safety
/// `source` must describe a buffer locked for reading for this call, with
/// `length` bytes readable from `start`.
unsafe fn copy_to_staging(
    device: &ID3D11Device,
    staging: &ID3D11Texture2D,
    source: Source,
    size: (u32, u32),
) -> Result<(), Error> {
    let pitch = usize::try_from(source.pitch).map_err(|_| Error::Geometry)?;
    let width = size.0 as usize;
    let height = size.1 as usize;
    if source.scanline.is_null() || source.start.is_null() || pitch < width {
        return Err(Error::Geometry);
    }
    // The planes must lie inside the locked range: scanline is at or after
    // the buffer start, and luma plus chroma rows end within its length.
    let offset = (source.scanline as usize)
        .checked_sub(source.start as usize)
        .ok_or(Error::Geometry)?;
    let rows = source.rows as usize;
    let chroma_start = rows.checked_mul(pitch).ok_or(Error::Geometry)?;
    let needed = rows
        .checked_add(rows / 2)
        .and_then(|total| total.checked_mul(pitch))
        .and_then(|bytes| bytes.checked_add(offset))
        .ok_or(Error::Geometry)?;
    if needed > source.length as usize || height > rows {
        return Err(Error::Geometry);
    }
    // SAFETY: (caller) the locked range covers `needed` bytes from `start`,
    // which contains every row read below.
    let plane = |first: usize, row: usize| unsafe {
        core::slice::from_raw_parts(source.scanline.add(first + row * pitch), width)
    };
    // SAFETY: Map yields a CPU-writable pointer to the staging subresource:
    // `height` luma rows then `height / 2` chroma rows of `RowPitch` bytes.
    unsafe {
        let context = device.GetImmediateContext().map_err(|_| Error::Codec)?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(staging, 0, D3D11_MAP_WRITE, 0, Some(&raw mut mapped))
            .map_err(|_| Error::Codec)?;
        let target_pitch = mapped.RowPitch as usize;
        if mapped.pData.is_null() || target_pitch < width {
            context.Unmap(staging, 0);
            return Err(Error::Geometry);
        }
        let target = mapped.pData.cast::<u8>();
        for row in 0..height {
            core::ptr::copy_nonoverlapping(
                plane(0, row).as_ptr(),
                target.add(row * target_pitch),
                width,
            );
        }
        let chroma_target = target.add(height * target_pitch);
        for row in 0..height / 2 {
            core::ptr::copy_nonoverlapping(
                plane(chroma_start, row).as_ptr(),
                chroma_target.add(row * target_pitch),
                width,
            );
        }
        context.Unmap(staging, 0);
    }
    Ok(())
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
        // Do not build an unbounded Rust vector or activate an unbounded
        // number of codecs. All COM references still need releasing when an
        // oversized native enumeration is refused.
        let mut chosen = None;
        for index in 0..count as usize {
            let activation = (*activates.add(index)).take();
            if count <= MAX_TRANSFORMS && chosen.is_none() {
                chosen =
                    activation.and_then(|activate| activate.ActivateObject::<IMFTransform>().ok());
            }
        }
        CoTaskMemFree(Some(activates.cast_const().cast()));
        chosen.ok_or(Error::Unavailable)
    }
}
