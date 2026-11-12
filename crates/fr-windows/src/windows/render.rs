//! The window's picture: a flip-model swap chain on the decoder's own D3D11
//! device, and the D3D11 video processor turning each decoded NV12 picture
//! into the BGRA back buffer. 1:1 whenever the picture fits the window
//! (`geometry::fit`); the picture never leaves the GPU.
use crate::geometry::{Rect, fit};
use core::mem::ManuallyDrop;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0,
    D3D11_VIDEO_COLOR_RGBA, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11Texture2D, ID3D11VideoContext1,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter, IDXGIDevice,
    IDXGIFactory2, IDXGISwapChain1,
};
use windows::core::{Interface, Result};

struct Conversion {
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input: (u32, u32),
    output: (u32, u32),
}

pub struct Renderer {
    video: ID3D11VideoDevice,
    context: ID3D11VideoContext1,
    swap_chain: IDXGISwapChain1,
    size: (u32, u32),
    conversion: Option<Conversion>,
}

impl Renderer {
    /// A swap chain for `hwnd`'s client area (`size`) on `device`.
    pub fn new(device: &ID3D11Device, hwnd: HWND, size: (u32, u32)) -> Result<Self> {
        // SAFETY: COM calls on a live device and window; every returned
        // interface is owned by its wrapper.
        unsafe {
            let adapter: IDXGIAdapter = device.cast::<IDXGIDevice>()?.GetAdapter()?;
            let factory: IDXGIFactory2 = adapter.GetParent()?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: size.0,
                Height: size.1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: 0,
            };
            let swap_chain =
                factory.CreateSwapChainForHwnd(device, hwnd, &raw const desc, None, None)?;
            let context = device
                .GetImmediateContext()?
                .cast::<ID3D11VideoContext1>()?;
            Ok(Self {
                video: device.cast()?,
                context,
                swap_chain,
                size,
                conversion: None,
            })
        }
    }

    /// The window's client area changed size.
    pub fn resize(&mut self, size: (u32, u32)) -> Result<()> {
        if size == self.size || size.0 == 0 || size.1 == 0 {
            return Ok(());
        }
        self.conversion = None;
        // SAFETY: no back-buffer reference outlives `draw`, as ResizeBuffers
        // requires.
        unsafe {
            self.swap_chain.ResizeBuffers(
                0,
                size.0,
                size.1,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            )?;
        }
        self.size = size;
        Ok(())
    }

    pub const fn size(&self) -> (u32, u32) {
        self.size
    }

    fn conversion(&mut self, input: (u32, u32)) -> Result<&Conversion> {
        let current = self
            .conversion
            .as_ref()
            .is_some_and(|p| p.input == input && p.output == self.size);
        if !current {
            let rate = DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: input.0,
                InputHeight: input.1,
                OutputFrameRate: rate,
                OutputWidth: self.size.0,
                OutputHeight: self.size.1,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            // SAFETY: COM calls on the live video device with a valid
            // content description.
            let (enumerator, processor) = unsafe {
                let enumerator = self.video.CreateVideoProcessorEnumerator(&raw const desc)?;
                let processor = self.video.CreateVideoProcessor(&enumerator, 0)?;
                (enumerator, processor)
            };
            self.conversion = Some(Conversion {
                enumerator,
                processor,
                input,
                output: self.size,
            });
        }
        Ok(self.conversion.as_ref().expect("set above"))
    }

    /// Converts slice `slice` of `texture` (the `picture`-sized visible part
    /// of a decoded NV12 surface) into the back buffer and presents it on
    /// the next vertical blank. Returns where the picture landed.
    #[allow(clippy::too_many_lines, reason = "one ordered sequence of COM calls")]
    pub fn draw(
        &mut self,
        texture: &ID3D11Texture2D,
        slice: u32,
        picture: (u32, u32),
        full_range: bool,
    ) -> Result<Option<Rect>> {
        let Some(dest) = fit(picture, self.size) else {
            return Ok(None);
        };
        let size = self.size;
        let video = self.video.clone();
        let context = self.context.clone();
        let swap_chain = self.swap_chain.clone();
        let Conversion {
            enumerator,
            processor,
            ..
        } = self.conversion(picture)?;
        let rect = |left: i32, top: i32, right: i32, bottom: i32| RECT {
            left,
            top,
            right,
            bottom,
        };
        let to_i32 = |v: u32| i32::try_from(v).unwrap_or(i32::MAX);
        // SAFETY: COM calls on live objects of one device; the back buffer
        // and both views are released before this function returns, so a
        // later ResizeBuffers holds no outstanding reference.
        unsafe {
            let back_buffer: ID3D11Texture2D = swap_chain.GetBuffer(0)?;
            let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut output = None;
            video.CreateVideoProcessorOutputView(
                &back_buffer,
                enumerator,
                &raw const output_desc,
                Some(&raw mut output),
            )?;
            let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: slice,
                    },
                },
            };
            let mut input = None;
            video.CreateVideoProcessorInputView(
                texture,
                enumerator,
                &raw const input_desc,
                Some(&raw mut input),
            )?;
            context.VideoProcessorSetStreamFrameFormat(
                processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            context.VideoProcessorSetStreamColorSpace1(
                processor,
                0,
                if full_range {
                    DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709
                } else {
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709
                },
            );
            context.VideoProcessorSetOutputColorSpace1(
                processor,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            // No driver "enhancement": the host's pixels, converted only.
            context.VideoProcessorSetStreamAutoProcessingMode(processor, 0, false);
            let source = rect(0, 0, to_i32(picture.0), to_i32(picture.1));
            context.VideoProcessorSetStreamSourceRect(processor, 0, true, Some(&raw const source));
            let target = rect(dest.left, dest.top, dest.right, dest.bottom);
            context.VideoProcessorSetStreamDestRect(processor, 0, true, Some(&raw const target));
            let whole = rect(0, 0, to_i32(size.0), to_i32(size.1));
            context.VideoProcessorSetOutputTargetRect(processor, true, Some(&raw const whole));
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
            context.VideoProcessorSetOutputBackgroundColor(processor, false, &raw const black);
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(input),
                ..Default::default()
            };
            let mut streams = [stream];
            let blt = context.VideoProcessorBlt(processor, output.as_ref(), 0, &streams);
            let [entry] = &mut streams;
            ManuallyDrop::drop(&mut entry.pInputSurface);
            blt?;
            drop(output);
            drop(back_buffer);
            swap_chain.Present(1, DXGI_PRESENT(0)).ok()?;
        }
        Ok(Some(dest))
    }
}
