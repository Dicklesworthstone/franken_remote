//! The X11 owner of the LOCAL pointer's image over the viewer window while
//! this client controls the host (plan §11.4). frd decides whether the local
//! pointer (with the host's confirmed shape) or the presenter's overlay renders
//! the remote pointer; this owner only applies that decision on the X server.
//!
//! One native thread and its own XCB connection, separate from input capture,
//! the renderer and codecs. It selects only Enter/Leave on the client's own
//! window: no grabs, no input, no injection. Requests are validated before any
//! allocation, replace at most one pending image, and are acknowledged only
//! after a checked X request. A failure is non-fatal to control: the owner
//! stops, restores the platform pointer and reports `stopped`, and frd stops
//! managing the local pointer. Xvfb is the only qualified server here: no
//! compositor, `HiDPI` cursor scaling or Wayland claim.
use crate::viewer_input::Window;
use fr_media::cursor::check_geometry;
use frd::session_startup::{
    ViewerControl,
    viewer_local_cursor::{Image, LocalCursor, LocalPointer, Refused, State},
};
use std::{
    ffi::{CString, c_char, c_int, c_void},
    ptr::NonNull,
    sync::{
        Arc, Mutex, TryLockError,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const TURN: Duration = Duration::from_millis(5);
const TURN_EVENTS: usize = 64;
unsafe extern "C" {
    fn fr_viewer_cursor_open(
        display: *const c_char,
        window: u32,
        width: u32,
        height: u32,
        inside: *mut c_int,
    ) -> *mut c_void;
    fn fr_viewer_cursor_close(handle: *mut c_void);
    fn fr_viewer_cursor_poll(handle: *mut c_void, inside: *mut c_int) -> c_int;
    fn fr_viewer_cursor_apply(
        handle: *mut c_void,
        kind: u32,
        width: u32,
        height: u32,
        hot_x: u32,
        hot_y: u32,
        argb: *const u32,
    ) -> c_int;
}

/// One validated request: premultiplied `0xAARRGGBB`, exactly `width*height`.
enum Pending {
    Default,
    Blank,
    Shape {
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
        argb: Vec<u32>,
    },
}

/// Straight-alpha RGBA8 to premultiplied ARGB32. Geometry, hotspot and the
/// exact length are checked BEFORE the bounded (≤ 256×256) allocation.
pub(crate) fn premultiplied(
    width: u16,
    height: u16,
    hotspot_x: u16,
    hotspot_y: u16,
    rgba: &[u8],
) -> Result<Vec<u32>, Refused> {
    let bytes = check_geometry(width, height, hotspot_x, hotspot_y, rgba.len())
        .map_err(|_| Refused::Invalid)?;
    let mut argb = Vec::new();
    argb.try_reserve_exact(bytes / 4)
        .map_err(|_| Refused::Invalid)?;
    for p in rgba.as_chunks::<4>().0 {
        let a = u32::from(p[3]);
        let scale = |c: u8| (u32::from(c) * a + 127) / 255;
        argb.push((a << 24) | (scale(p[0]) << 16) | (scale(p[1]) << 8) | scale(p[2]));
    }
    Ok(argb)
}

struct Shared {
    viewer: ViewerControl,
    stop: AtomicBool,
    stopped: AtomicBool,
    /// 0 unknown, 1 inside, 2 outside.
    pointer: AtomicU8,
    applied: AtomicU64,
    generation: AtomicU64,
    pending: Mutex<Option<(u64, Pending)>>,
}
impl Shared {
    fn live(&self) -> bool {
        !self.stop.load(Ordering::Acquire) && !self.viewer.is_stopped()
    }
}

/// Retained by the input capture attachment; its thread is joined with it.
pub struct X11WindowCursor {
    shared: Arc<Shared>,
    task: Option<JoinHandle<()>>,
}
/// The nonblocking frd-side handle; it owns no thread.
pub struct Handle(Arc<Shared>);

impl X11WindowCursor {
    /// Start on the application's LOCAL renderer window (never a peer XID).
    /// The owner also stops when the ORIGINAL controlled viewer is fenced.
    pub fn start(
        display: &str,
        window: Window,
        viewer: ViewerControl,
    ) -> Result<(Self, Handle), Refused> {
        let display = CString::new(display).map_err(|_| Refused::Invalid)?;
        let shared = Arc::new(Shared {
            viewer,
            stop: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            pointer: AtomicU8::new(0),
            applied: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            pending: Mutex::new(None),
        });
        let native = shared.clone();
        let task = thread::Builder::new()
            .name("fr-viewer-cursor".into())
            .spawn(move || {
                run(&native, &display, window);
                native.stopped.store(true, Ordering::Release);
            })
            .map_err(|_| Refused::Stopped)?;
        Ok((
            Self {
                shared: shared.clone(),
                task: Some(task),
            },
            Handle(shared),
        ))
    }
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
    }
    /// Nonblocking; `true` once the native thread ended (it restores the
    /// platform pointer before exiting). Idempotent.
    pub fn finish(&mut self) -> bool {
        if self.task.as_ref().is_some_and(|task| !task.is_finished()) {
            return false;
        }
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
        true
    }
}
impl Drop for X11WindowCursor {
    fn drop(&mut self) {
        self.stop();
    }
}

impl LocalCursor for Handle {
    fn state(&self) -> State {
        State {
            pointer: match self.0.pointer.load(Ordering::Acquire) {
                1 => LocalPointer::Inside,
                2 => LocalPointer::Outside,
                _ => LocalPointer::Unknown,
            },
            applied: self.0.applied.load(Ordering::Acquire),
            stopped: self.0.stopped.load(Ordering::Acquire) || !self.0.live(),
        }
    }
    fn request(&mut self, image: Image<'_>) -> Result<u64, Refused> {
        if self.state().stopped {
            return Err(Refused::Stopped);
        }
        let pending = match image {
            Image::Default => Pending::Default,
            Image::Blank => Pending::Blank,
            Image::Shape {
                width,
                height,
                hotspot_x,
                hotspot_y,
                rgba,
            } => Pending::Shape {
                width,
                height,
                hotspot_x,
                hotspot_y,
                argb: premultiplied(width, height, hotspot_x, hotspot_y, rgba)?,
            },
        };
        let mut slot = match self.0.pending.try_lock() {
            Ok(slot) => slot,
            Err(TryLockError::WouldBlock) => return Err(Refused::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(Refused::Stopped),
        };
        let generation = self.0.generation.fetch_add(1, Ordering::AcqRel) + 1;
        *slot = Some((generation, pending));
        Ok(generation)
    }
    fn stop(&self) {
        self.0.stop.store(true, Ordering::Release);
    }
}

struct Native(NonNull<c_void>);
impl Drop for Native {
    fn drop(&mut self) {
        // SAFETY: unique C allocation, created and released on this thread.
        // Close restores the window's own (None) cursor before disconnecting.
        unsafe { fr_viewer_cursor_close(self.0.as_ptr()) };
    }
}
fn run(shared: &Shared, display: &CString, window: Window) {
    if !shared.live() {
        return;
    }
    let mut inside: c_int = 0;
    // SAFETY: valid NUL string and writable scalar for the call only; the
    // returned handle stays confined to this thread.
    let Some(handle) = NonNull::new(unsafe {
        fr_viewer_cursor_open(
            display.as_ptr(),
            window.id,
            window.width,
            window.height,
            &raw mut inside,
        )
    }) else {
        return;
    };
    let native = Native(handle);
    shared
        .pointer
        .store(if inside != 0 { 1 } else { 2 }, Ordering::Release);
    while shared.live() {
        for _ in 0..TURN_EVENTS {
            // SAFETY: unique owning thread, writable scalar, no callbacks.
            match unsafe { fr_viewer_cursor_poll(native.0.as_ptr(), &raw mut inside) } {
                0 => break,
                1 => shared
                    .pointer
                    .store(if inside != 0 { 1 } else { 2 }, Ordering::Release),
                _ => return,
            }
        }
        let request = match shared.pending.lock() {
            Ok(mut slot) => slot.take(),
            Err(_) => return,
        };
        if let Some((generation, image)) = request {
            let (kind, width, height, hot_x, hot_y, pixels) = match &image {
                Pending::Default => (0, 0, 0, 0, 0, std::ptr::null()),
                Pending::Blank => (1, 0, 0, 0, 0, std::ptr::null()),
                Pending::Shape {
                    width,
                    height,
                    hotspot_x,
                    hotspot_y,
                    argb,
                } => (
                    2,
                    u32::from(*width),
                    u32::from(*height),
                    u32::from(*hotspot_x),
                    u32::from(*hotspot_y),
                    argb.as_ptr(),
                ),
            };
            // SAFETY: `pixels` is null or exactly width*height u32 values that
            // outlive this synchronous call; geometry was checked on request.
            let applied = unsafe {
                fr_viewer_cursor_apply(native.0.as_ptr(), kind, width, height, hot_x, hot_y, pixels)
            };
            if applied != 1 {
                return;
            }
            shared.applied.store(generation, Ordering::Release);
        }
        thread::sleep(TURN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shapes_are_validated_before_allocation_and_premultiplied() {
        // Hostile geometry is refused without allocating the ARGB copy.
        assert_eq!(premultiplied(0, 1, 0, 0, &[]), Err(Refused::Invalid));
        assert_eq!(
            premultiplied(257, 1, 0, 0, &[0; 257 * 4]),
            Err(Refused::Invalid)
        );
        assert_eq!(premultiplied(2, 2, 0, 0, &[0; 15]), Err(Refused::Invalid));
        assert_eq!(premultiplied(2, 2, 2, 0, &[0; 16]), Err(Refused::Invalid));
        assert_eq!(premultiplied(2, 2, 0, 2, &[0; 16]), Err(Refused::Invalid));
        // Opaque magenta, 50% white, fully transparent colour.
        let rgba = [
            0xff, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0x80, 0x12, 0x34, 0x56, 0x00,
        ];
        assert_eq!(
            premultiplied(3, 1, 2, 0, &rgba).unwrap(),
            [0xffff_00ff, 0x8080_8080, 0x0000_0000]
        );
    }
}
