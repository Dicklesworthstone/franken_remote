//! One bounded cursor bitmap on the decoder's existing D3D11 device.
//! Only cursor pixels cross from CPU memory. The decoded picture and the
//! swap-chain back buffer never leave the GPU. Every target reference is
//! released before return so resize and closure retain their original bounds.
use crate::{
    cursor::{self, State},
    geometry::Rect,
};
use windows::{
    Win32::{
        Foundation::E_INVALIDARG,
        Graphics::{
            Direct2D::{
                Common::{
                    D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED,
                    D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
                },
                D2D1_ANTIALIAS_MODE_ALIASED, D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
                D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1CreateFactory,
                ID2D1Bitmap1, ID2D1DeviceContext, ID2D1Factory1, ID2D1Image,
            },
            Direct3D11::{ID3D11Device, ID3D11Texture2D},
            Dxgi::{Common::DXGI_FORMAT_B8G8R8A8_UNORM, IDXGIDevice, IDXGISurface},
        },
    },
    core::{Error, Interface, Result},
};

#[derive(Default)]
pub(super) struct Overlay {
    state: State,
    context: Option<ID2D1DeviceContext>,
    bitmap: Option<ID2D1Bitmap1>,
}

impl Overlay {
    pub(super) const fn state(&self) -> State {
        self.state
    }

    pub(super) fn clear(&mut self) {
        self.bitmap = None;
        self.state = State::default();
    }

    /// `pixels` is supplied only when the original shape identity changes.
    /// Motion/hide updates retain the same bitmap and allocate nothing native.
    pub(super) fn update(
        &mut self,
        device: &ID3D11Device,
        state: State,
        pixels: Option<&[u8]>,
    ) -> Result<()> {
        if state.shape.bytes().is_none() {
            if state.visible || pixels.is_some() {
                return Err(Error::from_hresult(E_INVALIDARG));
            }
            self.bitmap = None;
        } else if state.shape_key() != self.state.shape_key() || self.bitmap.is_none() {
            let pixels = pixels
                .filter(|bytes| Some(bytes.len()) == state.shape.bytes())
                .ok_or_else(|| Error::from_hresult(E_INVALIDARG))?;
            let context = self.context(device)?;
            let properties = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                ..Default::default()
            };
            // SAFETY: the checked image is at most 256x256 BGRA8; the exact
            // row pitch and source allocation remain alive for this copy.
            let bitmap = unsafe {
                context.CreateBitmap(
                    D2D_SIZE_U {
                        width: u32::from(state.shape.width),
                        height: u32::from(state.shape.height),
                    },
                    Some(pixels.as_ptr().cast()),
                    u32::from(state.shape.width) * 4,
                    &raw const properties,
                )?
            };
            // At most the old and replacement bounded bitmap coexist.
            self.bitmap = Some(bitmap);
        }
        self.state = state;
        Ok(())
    }

    fn context(&mut self, device: &ID3D11Device) -> Result<&ID2D1DeviceContext> {
        if self.context.is_none() {
            let dxgi = device.cast::<IDXGIDevice>()?;
            // SAFETY: all Direct2D objects belong to this UI thread and use
            // the decoder's BGRA-capable D3D11 device. COM wrappers own them.
            let context = unsafe {
                let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
                let device = factory.CreateDevice(&dxgi)?;
                let context = device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
                // The Win32 viewer already operates in physical pixels.
                context.SetDpi(96.0, 96.0);
                context
            };
            self.context = Some(context);
        }
        self.context
            .as_ref()
            .ok_or_else(|| Error::from_hresult(E_INVALIDARG))
    }

    pub(super) fn visible(
        &self,
        dest: Rect,
        attempt: u64,
        sequence: u64,
    ) -> bool {
        self.bitmap.is_some()
            && self.state.visible_on(attempt, sequence)
            && cursor::placement(self.state, dest).is_some()
    }

    pub(super) fn draw(
        &self,
        back_buffer: &ID3D11Texture2D,
        dest: Rect,
        attempt: u64,
        sequence: u64,
    ) -> Result<()> {
        if !self.state.visible_on(attempt, sequence) {
            return Ok(());
        }
        let Some(placement) = cursor::placement(self.state, dest) else {
            return Ok(());
        };
        let (Some(context), Some(bitmap)) = (&self.context, &self.bitmap) else {
            return Err(Error::from_hresult(E_INVALIDARG));
        };
        let surface = back_buffer.cast::<IDXGISurface>()?;
        let properties = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            ..Default::default()
        };
        let rect = |[left, top, right, bottom]: [f32; 4]| D2D_RECT_F {
            left,
            top,
            right,
            bottom,
        };
        let target_rect = rect(placement.destination);
        let clip = rect(placement.clip);
        // SAFETY: this UI-thread context draws only onto the live BGRA back
        // buffer. It borrows the bounded bitmap and checked rectangles; the
        // clip is balanced and the target is detached even if EndDraw fails.
        unsafe {
            let target =
                context.CreateBitmapFromDxgiSurface(&surface, Some(&raw const properties))?;
            context.SetTarget(&target);
            context.BeginDraw();
            context.PushAxisAlignedClip(&raw const clip, D2D1_ANTIALIAS_MODE_ALIASED);
            context.DrawBitmap(
                bitmap,
                Some(&raw const target_rect),
                1.0,
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                None,
                None,
            );
            context.PopAxisAlignedClip();
            let drawn = context.EndDraw(None, None);
            context.SetTarget(None::<&ID2D1Image>);
            drawn?;
        }
        Ok(())
    }
}
