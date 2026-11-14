//! Desktop Duplication of one output, on a D3D11 device created for the
//! adapter that drives it. The duplicated image is copied into an owned
//! texture and the frame released at once, so the compositor never waits on
//! this process. Desktop Duplication leaves the pointer out of the image; it
//! is drawn into a second, GDI-compatible copy before conversion.
use super::freshness::{Frame, Observation, Source};
use crate::NativeError;
use std::collections::HashMap;
use windows::Win32::Foundation::{E_ACCESSDENIED, HMODULE, POINT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_RESOURCE_MISC_GDI_COMPATIBLE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_ROTATE90,
    DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_UNSPECIFIED,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, IDXGISurface1,
};
use windows::Win32::Graphics::Gdi::DeleteObject;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, HICON, ICONINFO,
};
use windows::core::Interface;

/// One output (monitor) of one adapter. Coordinates and sizes are physical
/// pixels: listing makes the process per-monitor DPI aware first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WinDisplay {
    /// The adapter's LUID as (low part, high part).
    pub adapter: (u32, i32),
    /// The output's index on that adapter.
    pub output: u32,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    /// Effective DPI (96 is 100 %).
    pub dpi: u32,
    /// Clockwise rotation in degrees: 0, 90, 180 or 270.
    pub rotation: u32,
}
impl WinDisplay {
    /// A stable catalog handle: adapter LUID and output index.
    pub fn handle(&self) -> u128 {
        (u128::from(self.adapter.1.cast_unsigned()) << 64)
            | (u128::from(self.adapter.0) << 32)
            | u128::from(self.output)
    }
    /// `GeometryChanged` unless the same output still has this placement,
    /// size, scale and rotation.
    pub fn check(&self) -> Result<(), NativeError> {
        if displays()?.contains(self) {
            Ok(())
        } else {
            Err(NativeError::GeometryChanged)
        }
    }
}

/// Every output attached to the desktop, on every adapter.
pub fn displays() -> Result<Vec<WinDisplay>, NativeError> {
    // SAFETY: plain process-wide setting; failing because it is already set
    // (by an earlier call or the manifest) is fine.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    // SAFETY: DXGI enumeration with owned out values; every interface is
    // released by its wrapper.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(|_| NativeError::Unavailable)?;
        let mut found = Vec::new();
        for a in 0.. {
            let adapter = match factory.EnumAdapters1(a) {
                Ok(adapter) => adapter,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(_) => return Err(NativeError::Unavailable),
            };
            let luid = adapter
                .GetDesc1()
                .map_err(|_| NativeError::Unavailable)?
                .AdapterLuid;
            for o in 0.. {
                let output = match adapter.EnumOutputs(o) {
                    Ok(output) => output,
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(_) => return Err(NativeError::Unavailable),
                };
                let desc = output.GetDesc().map_err(|_| NativeError::Unavailable)?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let r = desc.DesktopCoordinates;
                let (Ok(width), Ok(height)) = (
                    u32::try_from(r.right - r.left),
                    u32::try_from(r.bottom - r.top),
                ) else {
                    continue;
                };
                let rotation = match desc.Rotation {
                    DXGI_MODE_ROTATION_ROTATE90 => 90,
                    DXGI_MODE_ROTATION_ROTATE180 => 180,
                    DXGI_MODE_ROTATION_ROTATE270 => 270,
                    _ => 0,
                };
                let (mut x, mut y) = (0, 0);
                let dpi =
                    if GetDpiForMonitor(desc.Monitor, MDT_EFFECTIVE_DPI, &raw mut x, &raw mut y)
                        .is_ok()
                        && x != 0
                    {
                        x
                    } else {
                        96
                    };
                found.push(WinDisplay {
                    adapter: (luid.LowPart, luid.HighPart),
                    output: o,
                    left: r.left,
                    top: r.top,
                    width,
                    height,
                    dpi,
                    rotation,
                });
            }
        }
        Ok(found)
    }
}

/// The pointer as last drawn: shape handle, position on this display, shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pointer {
    shape: isize,
    x: i32,
    y: i32,
    visible: bool,
}

pub struct Duplicator {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    /// None after access loss, until the selected output is duplicated again.
    duplication: Option<IDXGIOutputDuplication>,
    /// The last duplicated desktop image, without the pointer.
    desktop: ID3D11Texture2D,
    /// `desktop` plus the pointer; the converter's input.
    composed: ID3D11Texture2D,
    display: WinDisplay,
    /// Evidence belongs to `duplication`, never to the cached GPU texture.
    source: Source,
    hotspots: HashMap<isize, POINT>,
    /// Desktop images acquired (new presents, not pointer-only updates).
    pub acquired: u64,
    /// Duplications recreated after `DXGI_ERROR_ACCESS_LOST`.
    pub recreated: u64,
    /// Times the input desktop refused a new duplication. Retained pixels
    /// cannot establish source progress while that desktop is unavailable.
    pub refused_desktops: u64,
}

fn texture(
    device: &ID3D11Device,
    (width, height): (u32, u32),
    gdi: bool,
) -> Result<ID3D11Texture2D, NativeError> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0).cast_unsigned(),
        CPUAccessFlags: 0,
        MiscFlags: if gdi {
            D3D11_RESOURCE_MISC_GDI_COMPATIBLE.0.cast_unsigned()
        } else {
            0
        },
    };
    let mut texture = None;
    // SAFETY: a valid description and out pointer on a live device.
    unsafe { device.CreateTexture2D(&raw const desc, None, Some(&raw mut texture)) }
        .map_err(|_| NativeError::Allocation)?;
    texture.ok_or(NativeError::Allocation)
}

impl Duplicator {
    /// Duplicates `display` on its own adapter. Rotated displays are refused
    /// (`Unavailable`) until the picture is rotated back on the GPU.
    pub fn open(display: &WinDisplay) -> Result<Self, NativeError> {
        if display.rotation != 0 {
            return Err(NativeError::Unavailable);
        }
        // SAFETY: DXGI/D3D11 creation with valid out pointers; every
        // interface is owned by its wrapper.
        let (device, context, output) = unsafe {
            let factory: IDXGIFactory1 =
                CreateDXGIFactory1().map_err(|_| NativeError::Unavailable)?;
            let adapter = adapter(&factory, display.adapter)?;
            let output: IDXGIOutput1 = adapter
                .EnumOutputs(display.output)
                .map_err(|_| NativeError::DisplayUnavailable)?
                .cast()
                .map_err(|_| NativeError::Unavailable)?;
            let levels: [D3D_FEATURE_LEVEL; 2] = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                None,
                Some(&raw mut context),
            )
            .map_err(|_| NativeError::Unavailable)?;
            let device: ID3D11Device = device.ok_or(NativeError::Unavailable)?;
            // The encoder transform uses this device from its own threads.
            let _ = device
                .cast::<ID3D11Multithread>()
                .map_err(|_| NativeError::Unavailable)?
                .SetMultithreadProtected(true);
            (device, context.ok_or(NativeError::Unavailable)?, output)
        };
        // A share cannot begin on a desktop it may not see.
        let duplication = duplicate(&output, &device, display)?.ok_or(NativeError::Unavailable)?;
        let size = (display.width, display.height);
        Ok(Self {
            desktop: texture(&device, size, false)?,
            composed: texture(&device, size, true)?,
            device,
            context,
            output,
            duplication: Some(duplication),
            display: *display,
            source: Source::default(),
            hotspots: HashMap::new(),
            acquired: 0,
            recreated: 0,
            refused_desktops: 0,
        })
    }

    pub const fn device(&self) -> &ID3D11Device {
        &self.device
    }
    pub const fn composed(&self) -> &ID3D11Texture2D {
        &self.composed
    }
    pub const fn has_picture(&self) -> bool {
        self.source.has_image()
    }

    /// Takes the newest desktop image, waiting up to `wait_ms` for one.
    /// A timeout or pointer-only update can verify the existing image only
    /// while its original duplication remains valid. After access loss, one
    /// immediate replacement acquisition may supply a new image; a denied or
    /// still-empty replacement is unknown, never an unchanged observation.
    pub(super) fn poll(&mut self, wait_ms: u32) -> Result<Observation, NativeError> {
        for attempt in 0..2 {
            if self.duplication.is_none() && !self.reduplicate()? {
                return Ok(Observation::Unknown);
            }
            // A counted reference: the arms below may replace the field.
            let duplication = self.duplication.clone().ok_or(NativeError::Unavailable)?;
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            // Only the initial acquisition may wait. Interface recreation
            // does not restart or lengthen this request's wait budget.
            let wait = if attempt == 0 { wait_ms } else { 0 };
            // SAFETY: valid out pointers on the live duplication; an acquired
            // frame is released below before any other duplication call.
            let acquired =
                unsafe { duplication.AcquireNextFrame(wait, &raw mut info, &raw mut resource) };
            match acquired {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                    return Ok(self.source.unchanged());
                }
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    self.source.invalidate();
                    self.duplication = None;
                    continue;
                }
                Err(_) => {
                    self.source.invalidate();
                    return Err(NativeError::Unavailable);
                }
            }
            let copied = (|| {
                if info.LastPresentTime == 0 {
                    return Ok(false);
                }
                let image: ID3D11Texture2D = resource
                    .as_ref()
                    .ok_or(NativeError::Unavailable)?
                    .cast()
                    .map_err(|_| NativeError::Unavailable)?;
                // SAFETY: both textures are BGRA of the display's size on the
                // same device; the copy is queued before the frame is released.
                unsafe { self.context.CopyResource(&self.desktop, &image) };
                Ok(true)
            })();
            drop(resource);
            // SAFETY: the frame acquired above, released exactly once, also
            // when copying failed or the callback contained only a pointer.
            let released = unsafe { duplication.ReleaseFrame() };
            let frame = match &copied {
                Ok(true) => Frame::Image,
                Ok(false) => Frame::PointerOnly,
                Err(_) => Frame::Failed,
            };
            let observation = self.source.released(frame, released.is_ok());
            let copied = copied?;
            match released {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    // Even a completed CopyResource belongs to the lost
                    // duplication; its pixels were invalidated above.
                    self.duplication = None;
                    continue;
                }
                Err(_) => return Err(NativeError::Unavailable),
            }
            if copied {
                self.acquired += 1;
            }
            return Ok(observation);
        }
        Ok(Observation::Unknown)
    }

    /// Replaces a lost duplication of this output. Successful recreation
    /// alone is not source evidence: its first desktop image is still owed.
    /// A refusing desktop is never followed or represented by cached pixels.
    fn reduplicate(&mut self) -> Result<bool, NativeError> {
        self.source.invalidate();
        let Some(duplication) = duplicate(&self.output, &self.device, &self.display)? else {
            self.refused_desktops += 1;
            return Ok(false);
        };
        self.duplication = Some(duplication);
        self.recreated += 1;
        Ok(true)
    }

    /// Where the pointer is now, relative to this display.
    pub fn pointer(&self) -> Pointer {
        let mut info = CURSORINFO {
            cbSize: u32::try_from(size_of::<CURSORINFO>()).unwrap_or(0),
            ..Default::default()
        };
        // SAFETY: a sized, writable CURSORINFO.
        let shown = unsafe { GetCursorInfo(&raw mut info) }.is_ok()
            && info.flags.0 & CURSOR_SHOWING.0 != 0
            && !info.hCursor.is_invalid();
        let (x, y) = (
            info.ptScreenPos.x - self.display.left,
            info.ptScreenPos.y - self.display.top,
        );
        let inside = (0..i32::try_from(self.display.width).unwrap_or(i32::MAX)).contains(&x)
            && (0..i32::try_from(self.display.height).unwrap_or(i32::MAX)).contains(&y);
        if shown && inside {
            Pointer {
                shape: info.hCursor.0 as isize,
                x,
                y,
                visible: true,
            }
        } else {
            Pointer {
                shape: 0,
                x: 0,
                y: 0,
                visible: false,
            }
        }
    }

    fn hotspot(&mut self, shape: isize) -> POINT {
        *self.hotspots.entry(shape).or_insert_with(|| {
            let mut info = ICONINFO::default();
            // SAFETY: a live shared cursor handle and a writable ICONINFO;
            // the bitmaps it returns are this process's to delete.
            unsafe {
                if GetIconInfo(HICON(shape as *mut _), &raw mut info).is_err() {
                    return POINT::default();
                }
                if !info.hbmMask.is_invalid() {
                    let _ = DeleteObject(info.hbmMask.into());
                }
                if !info.hbmColor.is_invalid() {
                    let _ = DeleteObject(info.hbmColor.into());
                }
            }
            POINT {
                x: i32::try_from(info.xHotspot).unwrap_or(0),
                y: i32::try_from(info.yHotspot).unwrap_or(0),
            }
        })
    }

    /// The converter's input: the last desktop image with `pointer` drawn
    /// in by GDI (every cursor kind: colour, alpha, monochrome, inverting).
    pub fn compose(&mut self, pointer: Pointer) -> Result<(), NativeError> {
        // SAFETY: same-size BGRA textures on one device.
        unsafe { self.context.CopyResource(&self.composed, &self.desktop) };
        if !pointer.visible {
            return Ok(());
        }
        let hot = self.hotspot(pointer.shape);
        let surface: IDXGISurface1 = self.composed.cast().map_err(|_| NativeError::Unavailable)?;
        // SAFETY: the GDI-compatible texture is not bound anywhere while its
        // DC is held; the DC is released before the texture is used again.
        unsafe {
            let dc = surface.GetDC(false).map_err(|_| NativeError::Unavailable)?;
            let drawn = DrawIconEx(
                dc,
                pointer.x - hot.x,
                pointer.y - hot.y,
                HICON(pointer.shape as *mut _),
                0,
                0,
                0,
                None,
                DI_NORMAL,
            );
            surface
                .ReleaseDC(None)
                .map_err(|_| NativeError::Unavailable)?;
            drawn.map_err(|_| NativeError::Unavailable)?;
        }
        Ok(())
    }
}

fn adapter(factory: &IDXGIFactory1, luid: (u32, i32)) -> Result<IDXGIAdapter1, NativeError> {
    for a in 0.. {
        // SAFETY: enumeration on a live factory.
        let adapter = match unsafe { factory.EnumAdapters1(a) } {
            Ok(adapter) => adapter,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(_) => return Err(NativeError::Unavailable),
        };
        // SAFETY: a live adapter.
        let desc = unsafe { adapter.GetDesc1() }.map_err(|_| NativeError::Unavailable)?;
        if (desc.AdapterLuid.LowPart, desc.AdapterLuid.HighPart) == luid {
            return Ok(adapter);
        }
    }
    Err(NativeError::DisplayUnavailable)
}

/// A duplication whose mode still matches `display`, or None while the
/// input desktop is not one this process may duplicate (`E_ACCESSDENIED`: the
/// secure desktop of a UAC prompt or Ctrl+Alt+Del, or another desktop).
fn duplicate(
    output: &IDXGIOutput1,
    device: &ID3D11Device,
    display: &WinDisplay,
) -> Result<Option<IDXGIOutputDuplication>, NativeError> {
    // SAFETY: a live output and device. Too many duplications and a
    // non-interactive session refuse here too.
    let duplication = match unsafe { output.DuplicateOutput(device) } {
        Ok(duplication) => duplication,
        Err(e) if e.code() == E_ACCESSDENIED => return Ok(None),
        Err(_) => return Err(NativeError::Unavailable),
    };
    // SAFETY: a live duplication.
    let desc = unsafe { duplication.GetDesc() };
    let rotated = !matches!(
        desc.Rotation,
        DXGI_MODE_ROTATION_IDENTITY | DXGI_MODE_ROTATION_UNSPECIFIED
    );
    if rotated || desc.ModeDesc.Width != display.width || desc.ModeDesc.Height != display.height {
        return Err(NativeError::GeometryChanged);
    }
    Ok(Some(duplication))
}
