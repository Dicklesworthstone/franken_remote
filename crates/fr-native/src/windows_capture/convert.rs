//! BGRA to NV12 on the capture adapter's D3D11 video processor: full-range
//! RGB in, BT.709 studio-range YCbCr out (the baseline `ColorInfo`), into a
//! texture of the coded size. The rows and columns past the visible picture
//! are the processor's black background, so the encoder's padding is never
//! uninitialised memory.
use crate::NativeError;
use core::mem::ManuallyDrop;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11Texture2D,
    ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

pub struct Converter {
    context: ID3D11VideoContext1,
    _enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    output: ID3D11VideoProcessorOutputView,
    nv12: ID3D11Texture2D,
}

const fn rect(width: u32, height: u32) -> RECT {
    RECT {
        left: 0,
        top: 0,
        right: width.cast_signed(),
        bottom: height.cast_signed(),
    }
}

impl Converter {
    /// Converts `source` (BGRA, `picture` sized) into an NV12 texture of
    /// `coded` size on `device`.
    #[allow(clippy::too_many_lines, reason = "one ordered processor setup")]
    pub fn open(
        device: &ID3D11Device,
        source: &ID3D11Texture2D,
        picture: (u32, u32),
        coded: (u32, u32),
        fps: u32,
    ) -> Result<Self, NativeError> {
        let fail = |_| NativeError::Unavailable;
        let rate = DXGI_RATIONAL {
            Numerator: fps,
            Denominator: 1,
        };
        let description = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: picture.0,
            InputHeight: picture.1,
            OutputFrameRate: rate,
            OutputWidth: coded.0,
            OutputHeight: coded.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
        };
        let nv12_desc = D3D11_TEXTURE2D_DESC {
            Width: coded.0,
            Height: coded.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0.cast_unsigned(),
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        // SAFETY: COM creation on a live device with valid descriptions and
        // out pointers; every interface is owned by its wrapper.
        unsafe {
            let video: ID3D11VideoDevice = device.cast().map_err(fail)?;
            let context: ID3D11VideoContext1 = device
                .GetImmediateContext()
                .map_err(fail)?
                .cast()
                .map_err(fail)?;
            let enumerator = video
                .CreateVideoProcessorEnumerator(&raw const description)
                .map_err(fail)?;
            let supports = |format, flag: i32| {
                enumerator
                    .CheckVideoProcessorFormat(format)
                    .is_ok_and(|bits| bits & flag.cast_unsigned() != 0)
            };
            if !supports(
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0,
            ) || !supports(
                DXGI_FORMAT_NV12,
                D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0,
            ) {
                return Err(NativeError::Unavailable);
            }
            let processor = video.CreateVideoProcessor(&enumerator, 0).map_err(fail)?;
            let mut nv12 = None;
            device
                .CreateTexture2D(&raw const nv12_desc, None, Some(&raw mut nv12))
                .map_err(|_| NativeError::Allocation)?;
            let nv12: ID3D11Texture2D = nv12.ok_or(NativeError::Allocation)?;
            let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut input = None;
            video
                .CreateVideoProcessorInputView(
                    source,
                    &enumerator,
                    &raw const input_desc,
                    Some(&raw mut input),
                )
                .map_err(fail)?;
            let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut output = None;
            video
                .CreateVideoProcessorOutputView(
                    &nv12,
                    &enumerator,
                    &raw const output_desc,
                    Some(&raw mut output),
                )
                .map_err(fail)?;
            context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            context.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            context.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );
            // No driver "enhancement": the desktop's pixels, converted only.
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            let visible = rect(picture.0, picture.1);
            context.VideoProcessorSetStreamSourceRect(
                &processor,
                0,
                true,
                Some(&raw const visible),
            );
            context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&raw const visible));
            let whole = rect(coded.0, coded.1);
            context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&raw const whole));
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 {
                    RGBA: D3D11_VIDEO_COLOR_RGBA {
                        R: 0.0,
                        G: 0.0,
                        B: 0.0,
                        A: 1.0,
                    },
                },
            };
            context.VideoProcessorSetOutputBackgroundColor(&processor, false, &raw const black);
            Ok(Self {
                context,
                _enumerator: enumerator,
                processor,
                input: input.ok_or(NativeError::Unavailable)?,
                output: output.ok_or(NativeError::Unavailable)?,
                nv12,
            })
        }
    }

    /// The NV12 texture `convert` writes.
    pub const fn output(&self) -> &ID3D11Texture2D {
        &self.nv12
    }

    /// Converts the source texture's current contents.
    pub fn convert(&self) -> Result<(), NativeError> {
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
            ..Default::default()
        };
        let mut streams = [stream];
        // SAFETY: views and processor of one device; the stream's input view
        // reference is released right after the call.
        let blt = unsafe {
            self.context
                .VideoProcessorBlt(&self.processor, &self.output, 0, &streams)
        };
        let [entry] = &mut streams;
        // SAFETY: the reference added above, dropped exactly once.
        unsafe { ManuallyDrop::drop(&mut entry.pInputSurface) };
        blt.map_err(|_| NativeError::Codec)
    }
}
