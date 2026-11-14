//! Cursor-sized composition on the single owned desktop texture. The
//! duplication's own shape/position updates are sampled while its frame is
//! held, then committed only after `ReleaseFrame` succeeds. No global cursor
//! query can splice a newer shape into an older capture.
//!
//! A clean GPU underlay and one CPU-readable/writable patch are each fixed
//! at 256 x 256 BGRA. Only the clipped cursor rectangle crosses this mapping;
//! the full desktop stays on the GPU. Restoring the old underlay before
//! sampling the next one handles overlapping movement and invisible cursors.
use super::cursor_region::{Kind, MAX_BYTES, MAX_SIDE, Pointer, Position, Region, Shape, State};
use crate::NativeError;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_MAP_READ_WRITE,
    D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, IDXGIOutputDuplication,
};

pub(super) struct Update {
    position: Position,
    shape: Option<Shape>,
}

#[derive(Default)]
pub(super) struct Cursor {
    state: State,
    scratch: Option<Scratch>,
}

impl Cursor {
    pub(super) const fn pointer(&self) -> Pointer {
        self.state.pointer()
    }

    /// Called with exactly one acquired frame outstanding. The frame's
    /// metadata bounds the allocation before `GetFramePointerShape` writes it.
    pub(super) fn acquire(
        &self,
        duplication: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
    ) -> Result<Update, NativeError> {
        let position = if info.LastMouseUpdateTime == 0 {
            Position::Unchanged
        } else if info.PointerPosition.Visible.as_bool() {
            Position::Visible((
                info.PointerPosition.Position.x,
                info.PointerPosition.Position.y,
            ))
        } else {
            Position::Hidden
        };
        let missing = matches!(position, Position::Visible(_)) && !self.state.has_shape();
        let size = usize::try_from(info.PointerShapeBufferSize)
            .map_err(|_| NativeError::InvalidConfiguration)?;
        if size > MAX_BYTES || (size != 0 && info.LastMouseUpdateTime == 0) {
            return Err(NativeError::InvalidConfiguration);
        }
        let shape = if size != 0 || missing {
            let capacity = if size == 0 { MAX_BYTES } else { size };
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(capacity)
                .map_err(|_| NativeError::Allocation)?;
            bytes.resize(capacity, 0);
            let mut required = 0;
            let mut description = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            // SAFETY: the sized buffer is writable for its entire length;
            // both metadata outputs are initialized, live stack values. No
            // borrowed shape or frame pointer escapes this acquisition.
            unsafe {
                duplication.GetFramePointerShape(
                    u32::try_from(capacity).map_err(|_| NativeError::InvalidConfiguration)?,
                    bytes.as_mut_ptr().cast(),
                    &raw mut required,
                    &raw mut description,
                )
            }
            .map_err(|_| NativeError::Unavailable)?;
            let required =
                usize::try_from(required).map_err(|_| NativeError::InvalidConfiguration)?;
            if required == 0 || required > capacity {
                return Err(NativeError::InvalidConfiguration);
            }
            bytes.truncate(required);
            let kind = match description.Type {
                value if value == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0.cast_unsigned() => {
                    Kind::Monochrome
                }
                value if value == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0.cast_unsigned() => {
                    Kind::Color
                }
                value
                    if value
                        == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR
                            .0
                            .cast_unsigned() =>
                {
                    Kind::MaskedColor
                }
                _ => return Err(NativeError::InvalidConfiguration),
            };
            Some(
                Shape::new(
                    kind,
                    description.Width,
                    description.Height,
                    description.Pitch,
                    bytes,
                )
                .ok_or(NativeError::InvalidConfiguration)?,
            )
        } else {
            None
        };
        Ok(Update { position, shape })
    }

    pub(super) fn released(&mut self, update: Update, new_image: bool) -> Result<(), NativeError> {
        self.state
            .released(update.position, update.shape, new_image)
            .ok_or(NativeError::InvalidConfiguration)
    }

    pub(super) fn invalidate(&mut self) {
        self.state.invalidate();
    }

    pub(super) fn compose(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        desktop: &ID3D11Texture2D,
        size: (u32, u32),
    ) -> Result<(), NativeError> {
        if let Some(previous) = self.state.take_underlay() {
            let scratch = self.scratch.as_ref().ok_or(NativeError::Unavailable)?;
            copy(
                context,
                desktop,
                (previous.x, previous.y),
                &scratch.clean,
                local_box(previous),
            );
        }
        let Some((shape, region)) = self.state.visible(size) else {
            return Ok(());
        };
        if self.scratch.is_none() {
            self.scratch = Some(Scratch::open(device)?);
        }
        let scratch = self.scratch.as_ref().ok_or(NativeError::Allocation)?;
        let source = D3D11_BOX {
            left: region.x,
            top: region.y,
            front: 0,
            right: region.x + region.width,
            bottom: region.y + region.height,
            back: 1,
        };
        copy(context, &scratch.clean, (0, 0), desktop, source);
        copy(
            context,
            &scratch.patch,
            (0, 0),
            &scratch.clean,
            local_box(region),
        );
        scratch.blend(context, shape, region)?;
        copy(
            context,
            desktop,
            (region.x, region.y),
            &scratch.patch,
            local_box(region),
        );
        self.state.drawn(region);
        Ok(())
    }
}

const fn local_box(region: Region) -> D3D11_BOX {
    D3D11_BOX {
        left: 0,
        top: 0,
        front: 0,
        right: region.width,
        bottom: region.height,
        back: 1,
    }
}

fn copy(
    context: &ID3D11DeviceContext,
    destination: &ID3D11Texture2D,
    (x, y): (u32, u32),
    source: &ID3D11Texture2D,
    area: D3D11_BOX,
) {
    // SAFETY: same-device BGRA resources with one subresource. Region comes
    // only from checked shape/desktop clipping, fits the fixed scratch, and
    // no resource is mapped during any copy. The context preserves order.
    unsafe {
        context.CopySubresourceRegion(destination, 0, x, y, 0, source, 0, Some(&raw const area));
    };
}

struct Scratch {
    clean: ID3D11Texture2D,
    patch: ID3D11Texture2D,
}

impl Scratch {
    fn open(device: &ID3D11Device) -> Result<Self, NativeError> {
        let create = |staging| {
            let description = D3D11_TEXTURE2D_DESC {
                Width: MAX_SIDE,
                Height: MAX_SIDE,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: if staging {
                    D3D11_USAGE_STAGING
                } else {
                    D3D11_USAGE_DEFAULT
                },
                BindFlags: 0,
                CPUAccessFlags: if staging {
                    (D3D11_CPU_ACCESS_READ.0 | D3D11_CPU_ACCESS_WRITE.0).cast_unsigned()
                } else {
                    0
                },
                MiscFlags: 0,
            };
            let mut texture = None;
            // SAFETY: fixed pixel and byte bounds; initialized descriptor and
            // valid out pointer on the same device as the owned desktop.
            unsafe { device.CreateTexture2D(&raw const description, None, Some(&raw mut texture)) }
                .map_err(|_| NativeError::Allocation)?;
            texture.ok_or(NativeError::Allocation)
        };
        Ok(Self {
            clean: create(false)?,
            patch: create(true)?,
        })
    }

    fn blend(
        &self,
        context: &ID3D11DeviceContext,
        shape: &Shape,
        region: Region,
    ) -> Result<(), NativeError> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: an unmapped staging resource with CPU read/write access;
        // Map waits for the ordered copy of this cursor rectangle. The sole
        // owner never submits native calls while these pixels are mapped.
        unsafe {
            context.Map(
                &self.patch,
                0,
                D3D11_MAP_READ_WRITE,
                0,
                Some(&raw mut mapped),
            )
        }
        .map_err(|_| NativeError::Unavailable)?;
        let result = (|| {
            let pitch = usize::try_from(mapped.RowPitch).map_err(|_| NativeError::Unavailable)?;
            let row_bytes =
                usize::try_from(region.width * 4).map_err(|_| NativeError::Unavailable)?;
            let last = pitch
                .checked_mul(region.height.saturating_sub(1) as usize)
                .and_then(|offset| offset.checked_add(row_bytes))
                .ok_or(NativeError::Unavailable)?;
            if mapped.pData.is_null() || pitch < row_bytes || last > isize::MAX as usize {
                return Err(NativeError::Unavailable);
            }
            for row in 0..region.height {
                // SAFETY: Map owns the entire fixed-size patch until the
                // unconditional Unmap below. Only initialized pixels copied
                // above are borrowed; padding and untouched rows are never
                // read. Bounds and all offset arithmetic were checked above.
                let pixels = unsafe {
                    std::slice::from_raw_parts_mut(
                        mapped.pData.cast::<u8>().add(row as usize * pitch),
                        row_bytes,
                    )
                };
                shape
                    .blend_row(region.source_x, region.source_y + row, pixels)
                    .ok_or(NativeError::InvalidConfiguration)?;
            }
            Ok(())
        })();
        // SAFETY: the successful Map above, ended exactly once on both the
        // success and refusal paths, before any copy or texture destruction.
        unsafe { context.Unmap(&self.patch, 0) };
        result
    }
}
