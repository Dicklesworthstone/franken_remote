//! Desktop Duplication of one output, on a D3D11 device created for the
//! adapter that drives it. The duplicated image is copied into an owned
//! texture and the frame released at once, so the compositor never waits on
//! this process. Desktop Duplication leaves the pointer out of the image; it
//! is drawn into a second, GDI-compatible copy before conversion.
use crate::NativeError;
use std::collections::HashMap;
use std::time::{Duration, Instant};
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

/// How long the last image may stand while the input desktop refuses
/// duplication. A UAC prompt or the Ctrl+Alt+Del screen is answered at the
/// machine; a lock, user switch or logoff ends the share through frd's
/// session monitor long before this.
const REFUSED_DESKTOP_LIMIT: Duration = Duration::from_secs(60);
/// Spacing of duplication attempts while the desktop refuses them.
const REFUSED_DESKTOP_RETRY: Duration = Duration::from_millis(100);

pub struct Duplicator {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    /// None while the input desktop refuses duplication.
    duplication: Option<IDXGIOutputDuplication>,
    /// When the input desktop began refusing duplication, and the last try.
    refused: Option<(Instant, Instant)>,
    /// The last duplicated desktop image, without the pointer.
    desktop: ID3D11Texture2D,
    /// `desktop` plus the pointer; the converter's input.
    composed: ID3D11Texture2D,
    display: WinDisplay,
    has_picture: bool,
    hotspots: HashMap<isize, POINT>,
    /// Desktop images acquired (new presents, not pointer-only updates).
    pub acquired: u64,
    /// Duplications recreated after `DXGI_ERROR_ACCESS_LOST`.
    pub recreated: u64,
    /// Times the input desktop refused a new duplication (the secure
    /// desktop of a UAC prompt or Ctrl+Alt+Del) and the last image stood.
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
            refused: None,
            display: *display,
            has_picture: false,
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
        self.has_picture
    }

    /// Takes the newest desktop image, waiting up to `wait_ms` for one.
    /// `true` when the image changed since the previous call. A pointer-only
    /// update is not an image change; `pointer` reports it.
    pub fn poll(&mut self, wait_ms: u32) -> Result<bool, NativeError> {
        // A counted reference: the arms below may replace the field.
        let Some(duplication) = self.duplication.clone() else {
            self.reduplicate()?;
            return Ok(false);
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: valid out pointers on the live duplication; an acquired
        // frame is released below before any other duplication call.
        let acquired =
            unsafe { duplication.AcquireNextFrame(wait_ms, &raw mut info, &raw mut resource) };
        match acquired {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                // A desktop switch (secure desktop, lock), a mode change or a
                // full-screen exclusive application. A new duplication starts
                // with a full image; a changed mode ends the capture; a
                // desktop that refuses duplication is waited out.
                self.duplication = None;
                self.reduplicate()?;
                return Ok(false);
            }
            Err(_) => return Err(NativeError::Unavailable),
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
        // SAFETY: the frame acquired above, released exactly once.
        let released = unsafe { duplication.ReleaseFrame() };
        let copied = copied?;
        if released.is_err() {
            return Err(NativeError::Unavailable);
        }
        if copied {
            self.has_picture = true;
            self.acquired += 1;
        }
        Ok(copied)
    }

    /// Replaces a lost duplication. While the input desktop refuses one,
    /// the last image stands (no change is reported) and a new one is tried
    /// every `REFUSED_DESKTOP_RETRY`, for at most `REFUSED_DESKTOP_LIMIT`.
    /// Only this display's own desktop is ever duplicated: the refusing
    /// desktop is waited out, never followed.
    fn reduplicate(&mut self) -> Result<(), NativeError> {
        let now = Instant::now();
        if let Some((since, tried)) = &mut self.refused {
            if now.duration_since(*since) > REFUSED_DESKTOP_LIMIT {
                return Err(NativeError::Unavailable);
            }
            if now.duration_since(*tried) < REFUSED_DESKTOP_RETRY {
                return Ok(());
            }
            *tried = now;
        }
        match duplicate(&self.output, &self.device, &self.display)? {
            Some(duplication) => {
                self.duplication = Some(duplication);
                self.refused = None;
                self.recreated += 1;
            }
            None if self.refused.is_none() => {
                self.refused = Some((now, now));
                self.refused_desktops += 1;
            }
            None => {}
        }
        Ok(())
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
