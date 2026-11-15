//! Rotate a borrowed DXGI image directly into the one owned desktop, on the
//! capture adapter's GPU. The input view exists only during `write`, before
//! the caller releases the duplication frame. There is no retained borrowed
//! image, temporal history, extra full-frame texture or CPU fallback.
use super::rotation::{Layout, Rotation};
use crate::NativeError;
use core::mem::ManuallyDrop;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CAPS, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_ROTATION, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_RATE_CONVERSION_CAPS,
    D3D11_VIDEO_PROCESSOR_ROTATION_90, D3D11_VIDEO_PROCESSOR_ROTATION_180,
    D3D11_VIDEO_PROCESSOR_ROTATION_270, D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11Texture2D,
    ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorEnumerator1, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL,
};
use windows::core::Interface;

pub struct Upright {
    video: ID3D11VideoDevice,
    context: ID3D11VideoContext1,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output: ID3D11VideoProcessorOutputView,
}

/// Both axes were checked by `Layout` before any D3D allocation.
const fn rect((width, height): (u32, u32)) -> RECT {
    RECT {
        left: 0,
        top: 0,
        right: width.cast_signed(),
        bottom: height.cast_signed(),
    }
}

impl Upright {
    #[allow(clippy::too_many_lines, reason = "one ordered GPU rotation setup")]
    pub fn open(
        device: &ID3D11Device,
        destination: &ID3D11Texture2D,
        layout: Layout,
        fps: u32,
    ) -> Result<Self, NativeError> {
        let fail = |_| NativeError::Unavailable;
        let input_size = layout.acquired();
        let output_size = layout.upright();
        let rate = DXGI_RATIONAL {
            Numerator: fps,
            Denominator: 1,
        };
        let description = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: input_size.0,
            InputHeight: input_size.1,
            OutputFrameRate: rate,
            OutputWidth: output_size.0,
            OutputHeight: output_size.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
        };
        // SAFETY: one live device; all descriptions and out pointers are
        // valid for the calls. COM wrappers own every successful creation.
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
            let mut caps = D3D11_VIDEO_PROCESSOR_CAPS::default();
            enumerator
                .GetVideoProcessorCaps(&raw mut caps)
                .map_err(fail)?;
            let formats = enumerator
                .CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)
                .map_err(fail)?;
            let required = (D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0
                | D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0)
                .cast_unsigned();
            if caps.FeatureCaps
                & D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_ROTATION
                    .0
                    .cast_unsigned()
                == 0
                || formats & required != required
                || caps.MaxInputStreams == 0
                || caps.MaxStreamStates == 0
            {
                return Err(NativeError::Unavailable);
            }
            let formats1: ID3D11VideoProcessorEnumerator1 = enumerator.cast().map_err(fail)?;
            if !formats1
                .CheckVideoProcessorFormatConversion(
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                )
                .map_err(fail)?
                .as_bool()
            {
                return Err(NativeError::Unavailable);
            }
            // The transform consumes only this acquired image. Never adopt
            // a processor that requires retaining earlier or future frames.
            // Bound even driver-provided capability enumeration.
            let mut index = None;
            for candidate in 0..caps.RateConversionCapsCount.min(32) {
                let mut rate_caps = D3D11_VIDEO_PROCESSOR_RATE_CONVERSION_CAPS::default();
                enumerator
                    .GetVideoProcessorRateConversionCaps(candidate, &raw mut rate_caps)
                    .map_err(fail)?;
                if rate_caps.PastFrames == 0 && rate_caps.FutureFrames == 0 {
                    index = Some(candidate);
                    break;
                }
            }
            let processor = video
                .CreateVideoProcessor(&enumerator, index.ok_or(NativeError::Unavailable)?)
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
                    destination,
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
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            let rotation = match layout.rotation() {
                Rotation::Identity => D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
                Rotation::Clockwise90 => D3D11_VIDEO_PROCESSOR_ROTATION_90,
                Rotation::Clockwise180 => D3D11_VIDEO_PROCESSOR_ROTATION_180,
                Rotation::Clockwise270 => D3D11_VIDEO_PROCESSOR_ROTATION_270,
            };
            context.VideoProcessorSetStreamRotation(&processor, 0, true, rotation);
            // D3D specifies source rectangles before rotation, destination
            // rectangles after rotation. This is a 1:1 upright transform.
            let source = rect(input_size);
            let destination = rect(output_size);
            context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&raw const source));
            context.VideoProcessorSetStreamDestRect(
                &processor,
                0,
                true,
                Some(&raw const destination),
            );
            context.VideoProcessorSetOutputTargetRect(
                &processor,
                true,
                Some(&raw const destination),
            );
            Ok(Self {
                video,
                context,
                enumerator,
                processor,
                output: output.ok_or(NativeError::Unavailable)?,
            })
        }
    }

    /// Queue the transform while the caller still owns the acquired frame.
    /// No input texture or view survives this call, including on failure.
    pub fn write(&self, source: &ID3D11Texture2D) -> Result<(), NativeError> {
        let description = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
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
        // SAFETY: `source` is a validated acquired texture on this device;
        // the caller has not released the duplication frame.
        unsafe {
            self.video.CreateVideoProcessorInputView(
                source,
                &self.enumerator,
                &raw const description,
                Some(&raw mut input),
            )
        }
        .map_err(|_| NativeError::Unavailable)?;
        let input = input.ok_or(NativeError::Unavailable)?;
        let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(input)),
            ..Default::default()
        }];
        // SAFETY: one live processor and matching input/output views on the
        // same device. All GPU reads are queued before ReleaseFrame.
        let result = unsafe {
            self.context
                .VideoProcessorBlt(&self.processor, &self.output, 0, &streams)
        };
        // SAFETY: the one reference transferred above, dropped exactly once
        // whether the submission succeeded or failed.
        unsafe { ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        result.map_err(|_| NativeError::Unavailable)
    }
}

#[cfg(test)]
#[path = "upright/tests.rs"]
mod tests;
