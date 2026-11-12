//! A hardware HEVC encoder: the Media Foundation transform the GPU vendor
//! registers (Intel Quick Sync, NVIDIA NVENC, AMD AMF), enumerated for the
//! capture adapter so a hybrid laptop never encodes on the other GPU. It is
//! given the capture device through a DXGI device manager and reads each NV12
//! texture in place.
//!
//! Hardware transforms are asynchronous: they announce `METransformNeedInput`
//! and `METransformHaveOutput` through their event generator. A pump thread
//! blocks on that generator and forwards events, so every wait here has a
//! deadline and nothing polls. Low-latency mode with no reordering gives one
//! unit per picture; `encode` submits one picture and returns its unit.
use crate::NativeError;
use core::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Instant;
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVEncVideoLTRBufferControl,
    CODECAPI_AVEncVideoMaxNumRefFrame, CODECAPI_AVEncVideoOutputColorNominalRange,
    CODECAPI_AVEncVideoOutputColorPrimaries, CODECAPI_AVEncVideoOutputColorTransferFunction,
    CODECAPI_AVEncVideoOutputColorTransferMatrix, CODECAPI_AVLowLatencyMode, ICodecAPI,
    IMF2DBuffer, IMFActivate, IMFAttributes, IMFDXGIDeviceManager, IMFMediaEventGenerator,
    IMFMediaType, IMFSample, IMFShutdown, IMFTransform, MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS,
    MEError, METransformHaveOutput, METransformNeedInput, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE,
    MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
    MF_VERSION, MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
    MFNominalRange_16_235, MFOffset, MFSTARTUP_NOSOCKET, MFSampleExtension_CleanPoint, MFStartup,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnum2, MFVideoArea,
    MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709,
    MFVideoTransFunc_709, MFVideoTransferMatrix_BT709, eAVEncCommonRateControlMode_CBR,
    eAVEncH265VProfile_Main_420_8, eAVEncVideoColorNominalRange_16_235,
    eAVEncVideoColorPrimaries_BT709, eAVEncVideoColorTransferFunction_22_709,
    eAVEncVideoColorTransferMatrix_BT709,
};
use windows::Win32::Media::{TIMERR_NOERROR, timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Threading::{
    GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
    ProcessPowerThrottling, SetProcessInformation,
};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};
use windows::core::{GUID, Interface};

/// Which transform encodes, and which low-latency settings it refused.
/// Content-free: names and setting identifiers only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderIdentity {
    /// The transform's friendly name (`MFT_FRIENDLY_NAME_Attribute`).
    pub name: String,
    /// Settings the transform refused; it keeps its own value for each.
    pub refused: Vec<&'static str>,
    /// The transform allocates its own output samples.
    pub provides_samples: bool,
}

enum Event {
    NeedInput,
    HaveOutput,
    Failed,
    Ended,
}

/// The transform's event generator, moved to the pump thread. Media
/// Foundation event generators are free-threaded (any thread may call
/// `GetEvent`); this wrapper only carries the reference across.
struct Generator(IMFMediaEventGenerator);
// SAFETY: see above; the pump thread is the only user after the move.
unsafe impl Send for Generator {}

/// A one-millisecond system timer while an encoder is open. Intel's
/// transform waits for its GPU work on timer ticks: at Windows' default
/// 15.625 ms resolution every unit took two ticks (31.3 ms, at most 32 units
/// a second at 2256x1504 on Iris Plus), at 1 ms about 12 ms. Windows 11
/// ignores the request while the process shows no window unless the process
/// opts out of that throttling, which it does here.
struct FineTimer {
    began: bool,
}
impl FineTimer {
    fn begin() -> Self {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        // SAFETY: process-wide scheduling requests with a valid state
        // structure of the stated size; each successful begin is ended once
        // (`Drop`).
        unsafe {
            let _ = SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                (&raw const state).cast(),
                u32::try_from(size_of::<PROCESS_POWER_THROTTLING_STATE>()).unwrap_or(u32::MAX),
            );
            Self {
                began: timeBeginPeriod(1) == TIMERR_NOERROR,
            }
        }
    }
}
impl Drop for FineTimer {
    fn drop(&mut self) {
        if self.began {
            // SAFETY: ends the period this value began.
            unsafe { timeEndPeriod(1) };
        }
    }
}

pub struct Encoder {
    transform: IMFTransform,
    codec: Option<ICodecAPI>,
    events: Receiver<Event>,
    pump: Option<JoinHandle<()>>,
    _manager: IMFDXGIDeviceManager,
    input: u32,
    output: u32,
    output_bytes: u32,
    /// `METransformNeedInput` announcements not yet answered.
    credits: u32,
    time: i64,
    duration: i64,
    identity: EncoderIdentity,
    shut_down: bool,
    _timer: FineTimer,
}

fn startup() -> Result<(), NativeError> {
    static STARTED: OnceLock<bool> = OnceLock::new();
    // SAFETY: process-wide Media Foundation start, once; this thread joins
    // the multithreaded apartment (already being in one is fine).
    let started =
        *STARTED.get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.is_ok());
    // SAFETY: as above, per calling thread.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if !started || (com.is_err() && com != RPC_E_CHANGED_MODE) {
        return Err(NativeError::Unavailable);
    }
    Ok(())
}

const fn pack(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

fn variant(vt: windows::Win32::System::Variant::VARENUM, value: VARIANT_0_0_0) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: value,
            }),
        },
    }
}
fn ui4(value: u32) -> VARIANT {
    variant(VT_UI4, VARIANT_0_0_0 { ulVal: value })
}
fn yes() -> VARIANT {
    variant(
        VT_BOOL,
        VARIANT_0_0_0 {
            boolVal: VARIANT_TRUE,
        },
    )
}

/// Sets the baseline colour description (BT.709, studio range), frame size,
/// rate and progressive scan on a video media type.
fn describe(
    kind: &IMFMediaType,
    subtype: &GUID,
    coded: (u32, u32),
    fps: u32,
) -> windows::core::Result<()> {
    // SAFETY: attribute setters on a live media type.
    unsafe {
        kind.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        kind.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        kind.SetUINT64(&MF_MT_FRAME_SIZE, pack(coded.0, coded.1))?;
        kind.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        kind.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        kind.SetUINT32(
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_Progressive.0.cast_unsigned(),
        )?;
        kind.SetUINT32(
            &MF_MT_VIDEO_PRIMARIES,
            MFVideoPrimaries_BT709.0.cast_unsigned(),
        )?;
        kind.SetUINT32(
            &MF_MT_TRANSFER_FUNCTION,
            MFVideoTransFunc_709.0.cast_unsigned(),
        )?;
        kind.SetUINT32(
            &MF_MT_YUV_MATRIX,
            MFVideoTransferMatrix_BT709.0.cast_unsigned(),
        )?;
        kind.SetUINT32(
            &MF_MT_VIDEO_NOMINAL_RANGE,
            MFNominalRange_16_235.0.cast_unsigned(),
        )?;
    }
    Ok(())
}

/// The first hardware HEVC encoder registered for this adapter.
fn find(adapter: (u32, i32)) -> Result<IMFActivate, NativeError> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_HEVC,
    };
    // LUID as laid out in memory: LowPart then HighPart.
    let mut luid = [0_u8; 8];
    luid[..4].copy_from_slice(&adapter.0.to_le_bytes());
    luid[4..].copy_from_slice(&adapter.1.to_le_bytes());
    // SAFETY: enumeration with valid type infos and out pointers; the
    // returned array is CoTaskMem owned by this function, every element
    // taken (or released) before it is freed.
    unsafe {
        let mut attributes: Option<IMFAttributes> = None;
        MFCreateAttributes(&raw mut attributes, 1).map_err(|_| NativeError::Unavailable)?;
        let attributes = attributes.ok_or(NativeError::Unavailable)?;
        attributes
            .SetBlob(&MFT_ENUM_ADAPTER_LUID, &luid)
            .map_err(|_| NativeError::Unavailable)?;
        let mut array: *mut Option<IMFActivate> = core::ptr::null_mut();
        let mut count = 0_u32;
        MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&raw const input),
            Some(&raw const output),
            &attributes,
            &raw mut array,
            &raw mut count,
        )
        .map_err(|_| NativeError::Unavailable)?;
        if array.is_null() {
            return Err(NativeError::Unavailable);
        }
        let mut first = None;
        for i in 0..count as usize {
            let entry = core::ptr::read(array.add(i));
            if first.is_none() {
                first = entry;
            }
        }
        CoTaskMemFree(Some(array.cast()));
        first.ok_or(NativeError::Unavailable)
    }
}

fn pump(generator: Generator, events: &Sender<Event>) {
    let Generator(generator) = generator;
    // SAFETY: this thread's apartment; failure leaves GetEvent to refuse.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    loop {
        // SAFETY: a blocking wait on the live generator; it returns
        // MF_E_SHUTDOWN once the transform is shut down.
        let Ok(event) = (unsafe { generator.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) })
        else {
            let _ = events.send(Event::Ended);
            return;
        };
        // SAFETY: reads on a live event.
        let (kind, status) = unsafe { (event.GetType(), event.GetStatus()) };
        let failed = !matches!(status, Ok(s) if s.is_ok());
        let forward = match kind.map(u32::cast_signed) {
            Ok(k) if failed || k == MEError.0 => Event::Failed,
            Ok(k) if k == METransformNeedInput.0 => Event::NeedInput,
            Ok(k) if k == METransformHaveOutput.0 => Event::HaveOutput,
            Ok(_) => continue,
            Err(_) => Event::Failed,
        };
        if events.send(forward).is_err() {
            return;
        }
    }
}

impl Encoder {
    /// Opens the adapter's hardware HEVC encoder for `coded` NV12 pictures
    /// whose visible part is `picture`, at `fps` and `bitrate` (CBR).
    #[allow(clippy::too_many_lines, reason = "one ordered transform setup")]
    pub fn open(
        device: &ID3D11Device,
        adapter: (u32, i32),
        coded: (u32, u32),
        picture: (u32, u32),
        fps: u32,
        bitrate: u32,
    ) -> Result<Self, NativeError> {
        startup()?;
        let activate = find(adapter)?;
        let fail = |_| NativeError::Unavailable;
        let mut refused = Vec::new();
        let timer = FineTimer::begin();
        if !timer.began {
            refused.push("timer_1ms");
        }
        // SAFETY: COM calls on live objects with valid arguments; every
        // returned interface is owned by its wrapper and the friendly name
        // string is freed after it is copied.
        unsafe {
            let mut name = String::new();
            let mut text = windows::core::PWSTR::null();
            let mut length = 0;
            if activate
                .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &raw mut text, &raw mut length)
                .is_ok()
            {
                name = text.to_string().unwrap_or_default();
                CoTaskMemFree(Some(text.0.cast()));
            }
            let transform: IMFTransform = activate.ActivateObject().map_err(fail)?;
            let attributes = transform.GetAttributes().map_err(fail)?;
            // Hardware encoders are asynchronous; a synchronous one would
            // never raise the events this owner waits for.
            if attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
                return Err(NativeError::Unavailable);
            }
            attributes
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .map_err(fail)?;
            if attributes.SetUINT32(&MF_LOW_LATENCY, 1).is_err() {
                refused.push("MF_LOW_LATENCY");
            }
            let mut token = 0_u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&raw mut token, &raw mut manager).map_err(fail)?;
            let manager: IMFDXGIDeviceManager = manager.ok_or(NativeError::Unavailable)?;
            manager.ResetDevice(device, token).map_err(fail)?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .map_err(fail)?;
            let (mut inputs, mut outputs) = ([0_u32], [0_u32]);
            let (input, output) = if transform.GetStreamIDs(&mut inputs, &mut outputs).is_ok() {
                (inputs[0], outputs[0])
            } else {
                (0, 0)
            };
            // Rate control before the media types: some encoders fix their
            // mode when the output type is set.
            let api = transform.cast::<ICodecAPI>().ok();
            if let Some(api) = &api {
                let settings: [(&GUID, VARIANT, &'static str); 11] = [
                    (
                        &CODECAPI_AVEncCommonRateControlMode,
                        ui4(eAVEncCommonRateControlMode_CBR.0.cast_unsigned()),
                        "rate_control_cbr",
                    ),
                    (
                        &CODECAPI_AVEncCommonMeanBitRate,
                        ui4(bitrate),
                        "mean_bitrate",
                    ),
                    (&CODECAPI_AVLowLatencyMode, yes(), "low_latency"),
                    (
                        &CODECAPI_AVEncMPVDefaultBPictureCount,
                        ui4(0),
                        "no_b_pictures",
                    ),
                    // Pictures are IDR only when the host asks; the longest
                    // interval the setting holds otherwise.
                    (&CODECAPI_AVEncMPVGOPSize, ui4(u32::MAX), "gop_unbounded"),
                    // One reference and no long-term syntax where the
                    // transform allows it. Intel's refuses both (and the
                    // colour settings below) and emits low-delay B slices
                    // with two past references, which admission accepts.
                    (&CODECAPI_AVEncVideoMaxNumRefFrame, ui4(1), "one_reference"),
                    (&CODECAPI_AVEncVideoLTRBufferControl, ui4(0), "no_long_term"),
                    // The colour description the guard requires in the VUI;
                    // `WinCapture` writes it when the transform cannot.
                    (
                        &CODECAPI_AVEncVideoOutputColorPrimaries,
                        ui4(eAVEncVideoColorPrimaries_BT709.0.cast_unsigned()),
                        "primaries_bt709",
                    ),
                    (
                        &CODECAPI_AVEncVideoOutputColorTransferFunction,
                        ui4(eAVEncVideoColorTransferFunction_22_709.0.cast_unsigned()),
                        "transfer_bt709",
                    ),
                    (
                        &CODECAPI_AVEncVideoOutputColorTransferMatrix,
                        ui4(eAVEncVideoColorTransferMatrix_BT709.0.cast_unsigned()),
                        "matrix_bt709",
                    ),
                    (
                        &CODECAPI_AVEncVideoOutputColorNominalRange,
                        ui4(eAVEncVideoColorNominalRange_16_235.0.cast_unsigned()),
                        "range_16_235",
                    ),
                ];
                for (setting, value, label) in &settings {
                    if api.SetValue(*setting, value).is_err() {
                        refused.push(*label);
                    }
                }
            } else {
                refused.push("codec_api");
            }
            let out_type = MFCreateMediaType().map_err(fail)?;
            describe(&out_type, &MFVideoFormat_HEVC, coded, fps).map_err(fail)?;
            out_type
                .SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
                .map_err(fail)?;
            out_type
                .SetUINT32(
                    &MF_MT_MPEG2_PROFILE,
                    eAVEncH265VProfile_Main_420_8.0.cast_unsigned(),
                )
                .map_err(fail)?;
            let aperture = MFVideoArea {
                OffsetX: MFOffset::default(),
                OffsetY: MFOffset::default(),
                Area: windows::Win32::Foundation::SIZE {
                    cx: picture.0.cast_signed(),
                    cy: picture.1.cast_signed(),
                },
            };
            let aperture_bytes = core::slice::from_raw_parts(
                (&raw const aperture).cast::<u8>(),
                size_of::<MFVideoArea>(),
            );
            if out_type
                .SetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, aperture_bytes)
                .is_err()
            {
                refused.push("display_aperture");
            }
            transform
                .SetOutputType(output, &out_type, 0)
                .map_err(fail)?;
            let in_type = MFCreateMediaType().map_err(fail)?;
            describe(&in_type, &MFVideoFormat_NV12, coded, fps).map_err(fail)?;
            transform.SetInputType(input, &in_type, 0).map_err(fail)?;
            let info = transform.GetOutputStreamInfo(output).map_err(fail)?;
            let provides = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                    .cast_unsigned()
                != 0;
            let generator = Generator(transform.cast::<IMFMediaEventGenerator>().map_err(fail)?);
            let (sender, events) = mpsc::channel();
            let pump = std::thread::Builder::new()
                .name("fr-mf-events".into())
                .spawn(move || pump(generator, &sender))
                .map_err(|_| NativeError::Allocation)?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(fail)?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(fail)?;
            Ok(Self {
                transform,
                codec: api,
                events,
                pump: Some(pump),
                _manager: manager,
                input,
                output,
                output_bytes: info.cbSize.max(1 << 20),
                credits: 0,
                time: 0,
                duration: 10_000_000 / i64::from(fps.max(1)),
                identity: EncoderIdentity {
                    name,
                    refused,
                    provides_samples: provides,
                },
                shut_down: false,
                _timer: timer,
            })
        }
    }

    pub const fn identity(&self) -> &EncoderIdentity {
        &self.identity
    }

    fn next(&mut self, deadline: Instant) -> Result<Event, NativeError> {
        let left = deadline.saturating_duration_since(Instant::now());
        match self.events.recv_timeout(left) {
            Ok(Event::Failed) => Err(NativeError::Codec),
            Ok(Event::Ended) | Err(RecvTimeoutError::Disconnected) => Err(NativeError::Closed),
            Ok(event) => Ok(event),
            Err(RecvTimeoutError::Timeout) => Err(NativeError::NeedDrain),
        }
    }

    /// New mean bitrate; returns the value the transform reports back.
    pub fn set_bitrate(&self, bitrate: u32) -> Result<u32, NativeError> {
        let codec = self.codec.as_ref().ok_or(NativeError::Unavailable)?;
        // SAFETY: property calls on the live transform.
        unsafe {
            codec
                .SetValue(&CODECAPI_AVEncCommonMeanBitRate, &ui4(bitrate))
                .map_err(|_| NativeError::Unavailable)?;
            let value = codec
                .GetValue(&CODECAPI_AVEncCommonMeanBitRate)
                .map_err(|_| NativeError::Unavailable)?;
            Ok(value.Anonymous.Anonymous.Anonymous.ulVal)
        }
    }

    /// Encodes `nv12` and returns its Annex B unit and whether the transform
    /// marked it a clean point (IDR). `NeedDrain` when the transform gave
    /// nothing by `deadline`.
    /// Encodes the picture in `nv12`, captured at `micros` (the parent's
    /// clock): rate control spends bits over real time even when pictures
    /// arrive only as the desktop changes.
    pub fn encode(
        &mut self,
        nv12: &ID3D11Texture2D,
        force_idr: bool,
        micros: u64,
        deadline: Instant,
    ) -> Result<(Vec<u8>, bool), NativeError> {
        while self.credits == 0 {
            match self.next(deadline)? {
                Event::NeedInput => self.credits += 1,
                // An output before any input of this call would belong to an
                // earlier picture: the one-in, one-out contract broke.
                _ => return Err(NativeError::Codec),
            }
        }
        if force_idr {
            let codec = self.codec.as_ref().ok_or(NativeError::Unavailable)?;
            // SAFETY: a property call on the live transform.
            unsafe { codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &ui4(1)) }
                .map_err(|_| NativeError::Codec)?;
        }
        // 100 ns units, strictly increasing.
        let time = i64::try_from(micros.saturating_mul(10))
            .unwrap_or(i64::MAX)
            .max(self.time.saturating_add(1));
        // SAFETY: a DXGI surface buffer over the converter's NV12 texture
        // (alive until the unit returns: the transform has read the picture
        // by the time it outputs it) wrapped in a new sample.
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, nv12, 0, false)
                .map_err(|_| NativeError::Allocation)?;
            if let Ok(planes) = buffer.cast::<IMF2DBuffer>() {
                let length = planes
                    .GetContiguousLength()
                    .map_err(|_| NativeError::Codec)?;
                buffer
                    .SetCurrentLength(length)
                    .map_err(|_| NativeError::Codec)?;
            }
            let sample = MFCreateSample().map_err(|_| NativeError::Allocation)?;
            sample.AddBuffer(&buffer).map_err(|_| NativeError::Codec)?;
            sample.SetSampleTime(time).map_err(|_| NativeError::Codec)?;
            sample
                .SetSampleDuration(self.duration)
                .map_err(|_| NativeError::Codec)?;
            self.transform
                .ProcessInput(self.input, &sample, 0)
                .map_err(|_| NativeError::Codec)?;
        }
        self.credits -= 1;
        self.time = time;
        loop {
            match self.next(deadline)? {
                Event::NeedInput => self.credits += 1,
                Event::HaveOutput => {
                    if let Some(unit) = self.take()? {
                        return Ok(unit);
                    }
                }
                Event::Failed | Event::Ended => return Err(NativeError::Codec),
            }
        }
    }

    /// One `ProcessOutput`; `None` after an output type renegotiation.
    fn take(&mut self) -> Result<Option<(Vec<u8>, bool)>, NativeError> {
        let own = if self.identity.provides_samples {
            None
        } else {
            // SAFETY: a new memory sample of the transform's stated size.
            unsafe {
                let sample = MFCreateSample().map_err(|_| NativeError::Allocation)?;
                let buffer =
                    MFCreateMemoryBuffer(self.output_bytes).map_err(|_| NativeError::Allocation)?;
                sample
                    .AddBuffer(&buffer)
                    .map_err(|_| NativeError::Allocation)?;
                Some(sample)
            }
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.output,
            pSample: ManuallyDrop::new(own),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0_u32;
        // SAFETY: one output buffer for the live transform; the sample and
        // event references it holds afterwards are taken exactly once.
        let (result, sample) = unsafe {
            let result = self
                .transform
                .ProcessOutput(0, &mut buffers, &raw mut status);
            let [entry] = &mut buffers;
            drop(ManuallyDrop::take(&mut entry.pEvents));
            (result, ManuallyDrop::take(&mut entry.pSample))
        };
        match result {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // SAFETY: renegotiation on the live transform.
                unsafe {
                    let kind = self
                        .transform
                        .GetOutputAvailableType(self.output, 0)
                        .map_err(|_| NativeError::Codec)?;
                    self.transform
                        .SetOutputType(self.output, &kind, 0)
                        .map_err(|_| NativeError::Codec)?;
                }
                return Ok(None);
            }
            Err(_) => return Err(NativeError::Codec),
        }
        let sample: IMFSample = sample.ok_or(NativeError::Codec)?;
        // SAFETY: reads of a live output sample; the lock is released before
        // the copy leaves this block.
        unsafe {
            let clean = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|_| NativeError::Codec)?;
            let mut data = core::ptr::null_mut();
            let mut length = 0_u32;
            buffer
                .Lock(&raw mut data, None, Some(&raw mut length))
                .map_err(|_| NativeError::Codec)?;
            let bytes = if data.is_null() {
                Vec::new()
            } else {
                core::slice::from_raw_parts(data, length as usize).to_vec()
            };
            buffer.Unlock().map_err(|_| NativeError::Codec)?;
            if bytes.is_empty() {
                return Err(NativeError::UnsupportedBitstream);
            }
            Ok(Some((bytes, clean)))
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: end-of-stream messages and shutdown on the live transform.
        // The pump thread is joined only when shutdown ended its wait.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                self.shut_down = shutdown.Shutdown().is_ok();
            }
        }
        if self.shut_down
            && let Some(pump) = self.pump.take()
        {
            let _ = pump.join();
        }
    }
}
