//! `RandR` 1.5 monitor inventory and selected-rectangle capture on one X connection.
//!
//! This blocking, thread-confined owner belongs in a supervised native process,
//! never the broker's authority task. Xlib/server failure may terminate that
//! process. Names, EDID and connector strings never leave this boundary.
use crate::{BgraFrame, NativeError};
use core::{
    ffi::{c_char, c_int, c_long, c_ulong, c_void},
    marker::PhantomData,
    ptr::NonNull,
};
use fr_core::{ids::DisplayGeometryGeneration, limits::ProtocolLimits};
use fr_wire::display::{Catalog, Display, MAX_DISPLAYS};
use std::{ffi::CString, rc::Rc};

// The public libXrandr 1.5 ABI. Bool is C int (not Rust bool); XIDs/Atoms
// are unsigned long on the client even though the X wire carries 32 bits.
#[repr(C)]
struct MonitorInfo {
    name: c_ulong,
    primary: c_int,
    automatic: c_int,
    noutput: c_int,
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
    mwidth: c_int,
    mheight: c_int,
    outputs: *mut c_ulong,
}
#[repr(C)]
union Event {
    kind: c_int,
    padding: [c_long; 24],
}
#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(display: *mut c_void) -> c_int;
    fn XDefaultRootWindow(display: *mut c_void) -> c_ulong;
    fn XGetGeometry(
        display: *mut c_void,
        drawable: c_ulong,
        root: *mut c_ulong,
        x: *mut c_int,
        y: *mut c_int,
        width: *mut u32,
        height: *mut u32,
        border: *mut u32,
        depth: *mut u32,
    ) -> c_int;
    fn XSelectInput(display: *mut c_void, root: c_ulong, mask: c_long) -> c_int;
    fn XSync(display: *mut c_void, discard: c_int) -> c_int;
    fn XEventsQueued(display: *mut c_void, mode: c_int) -> c_int;
    fn XNextEvent(display: *mut c_void, event: *mut Event) -> c_int;
}
// An explicit system ABI dependency, not a downloaded library or peer-selected
// path. The opt-in linux-displays feature needs libXrandr.so.2 at runtime.
#[link(name = "libXrandr.so.2", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn XRRQueryExtension(display: *mut c_void, event: *mut c_int, error: *mut c_int) -> c_int;
    fn XRRQueryVersion(display: *mut c_void, major: *mut c_int, minor: *mut c_int) -> c_int;
    fn XRRSelectInput(display: *mut c_void, root: c_ulong, mask: c_int);
    fn XRRGetMonitors(
        display: *mut c_void,
        root: c_ulong,
        active: c_int,
        count: *mut c_int,
    ) -> *mut MonitorInfo;
    fn XRRFreeMonitors(monitors: *mut MonitorInfo);
}
unsafe extern "C" {
    fn fr_x11_capture_rectangle(
        display: *mut c_void,
        root: c_ulong,
        x: c_int,
        y: c_int,
        width: c_int,
        height: c_int,
        output: *mut u8,
        length: usize,
    ) -> c_int;
}

const MAX_OUTPUTS: usize = 8;
const MAX_EVENTS: usize = 128;
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Monitor {
    name: c_ulong,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    outputs: [c_ulong; MAX_OUTPUTS],
    count: usize,
}
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Snapshot {
    width: u32,
    height: u32,
    count: usize,
    monitors: [Monitor; MAX_DISPLAYS],
}
struct MonitorAllocation(*mut MonitorInfo);
impl Drop for MonitorAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer is the unmodified allocation returned by
            // XRRGetMonitors, freed exactly once after all copies complete.
            unsafe { XRRFreeMonitors(self.0) };
        }
    }
}

/// One bounded catalog and its original live X connection. Any observed topology
/// change retires this inventory, including remove/re-add with identical bounds.
/// A replacement inventory requires a new parent worker/session generation.
/// Logical units here are X root-window pixels, not guessed toolkit DPI units.
/// Capture is already in the root's orientation, so no second rotation applies.
pub struct X11Inventory {
    display: NonNull<c_void>,
    root: c_ulong,
    event_base: c_int,
    snapshot: Snapshot,
    catalog: Catalog,
    limits: ProtocolLimits,
    closed: bool,
    _thread: PhantomData<Rc<()>>,
}
impl X11Inventory {
    pub fn open(name: &str, limits: ProtocolLimits) -> Result<Self, NativeError> {
        if !local_selector(name) {
            return Err(NativeError::DisplayUnavailable);
        }
        crate::xlib::initialize_threads().map_err(|_| NativeError::DisplayUnavailable)?;
        let name = CString::new(name).map_err(|_| NativeError::DisplayUnavailable)?;
        // SAFETY: name is a valid, locally supplied, NUL-terminated selector.
        let display = NonNull::new(unsafe { XOpenDisplay(name.as_ptr()) })
            .ok_or(NativeError::DisplayUnavailable)?;
        // Establish the owner immediately so every later refusal closes Xlib.
        let mut this = Self {
            display,
            root: 0,
            event_base: 0,
            snapshot: Snapshot::default(),
            catalog: Catalog::new(1, &[], &limits)
                .map_err(|_| NativeError::InvalidConfiguration)?,
            limits,
            closed: false,
            _thread: PhantomData,
        };
        let (mut error, mut major, mut minor) = (0, 1, 5);
        // SAFETY: live, exclusively owned display; all out-pointers are valid.
        if unsafe { XRRQueryExtension(display.as_ptr(), &raw mut this.event_base, &raw mut error) }
            == 0
            || unsafe { XRRQueryVersion(display.as_ptr(), &raw mut major, &raw mut minor) } == 0
            || major != 1
            || minor < 5
        {
            return Err(NativeError::Unavailable);
        }
        // SAFETY: root belongs to this display; masks are RandR 1.5 subscription
        // bits for screen/CRTC/output/property/resource changes. No mutation RPC.
        this.root = unsafe { XDefaultRootWindow(display.as_ptr()) };
        unsafe {
            XRRSelectInput(display.as_ptr(), this.root, 1 | 2 | 4 | 8 | 64);
            // RandR monitor add/delete uses core ConfigureNotify on the root,
            // not just RRNotify. Retain it even when final metadata is equal.
            XSelectInput(display.as_ptr(), this.root, 1 << 17);
        };
        this.barrier(true)?;
        this.snapshot = this.read_snapshot()?;
        // A change during the snapshot is a refusal, not permission to discard
        // events and certify an incoherent new baseline.
        this.barrier(false)?;
        let mut entries = Vec::with_capacity(this.snapshot.count);
        for (i, m) in this.snapshot.monitors[..this.snapshot.count]
            .iter()
            .enumerate()
        {
            let display = Display {
                handle: (i as u128) + 1,
                geometry: DisplayGeometryGeneration::INITIAL,
                x: m.x,
                y: m.y,
                pixel_width: m.width,
                pixel_height: m.height,
                logical_width: m.width,
                logical_height: m.height,
                scale_numerator: 1,
                scale_denominator: 1,
                rotation: 0,
            };
            display
                .validate(&limits)
                .map_err(|_| NativeError::InvalidConfiguration)?;
            entries.push(display);
        }
        this.catalog =
            Catalog::new(1, &entries, &limits).map_err(|_| NativeError::InvalidConfiguration)?;
        Ok(this)
    }
    pub fn catalog(&mut self) -> Result<Catalog, NativeError> {
        self.revalidate()?;
        Ok(self.catalog)
    }
    pub fn revalidate(&mut self) -> Result<(), NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        let result = (|| {
            self.barrier(false)?;
            if self.read_snapshot()? != self.snapshot {
                return Err(NativeError::GeometryChanged);
            }
            self.barrier(false)
        })();
        if result.is_err() {
            self.closed = true;
        }
        result
    }
    pub fn select(mut self, handle: u128) -> Result<X11SelectedCapture, NativeError> {
        self.revalidate()?;
        let selected = self
            .catalog
            .find(handle)
            .ok_or(NativeError::InvalidConfiguration)?;
        super::linux::frame_len(selected.pixel_width, selected.pixel_height, &self.limits)?;
        Ok(X11SelectedCapture {
            inventory: self,
            selected,
        })
    }
    fn barrier(&mut self, initial: bool) -> Result<(), NativeError> {
        // SAFETY: sole thread-confined connection. XSync establishes a server
        // ordering barrier; False preserves all notifications for inspection.
        unsafe { XSync(self.display.as_ptr(), 0) };
        let queued = unsafe { XEventsQueued(self.display.as_ptr(), 0) };
        let queued = usize::try_from(queued).map_err(|_| NativeError::DisplayUnavailable)?;
        if queued > MAX_EVENTS {
            return Err(NativeError::GeometryChanged);
        }
        for _ in 0..queued {
            let mut event = Event { padding: [0; 24] };
            // SAFETY: queued events exist; Event is Xlib's public C union ABI.
            unsafe { XNextEvent(self.display.as_ptr(), &raw mut event) };
            let kind = unsafe { event.kind };
            if !initial && (kind == 22 || kind == self.event_base || kind == self.event_base + 1) {
                return Err(NativeError::GeometryChanged);
            }
        }
        Ok(())
    }
    fn read_snapshot(&self) -> Result<Snapshot, NativeError> {
        let mut result = Snapshot::default();
        let (mut root, mut x, mut y, mut border, mut depth) = (0, 0, 0, 0, 0);
        // SAFETY: live root and writable out-parameters; no borrowed data escapes.
        if unsafe {
            XGetGeometry(
                self.display.as_ptr(),
                self.root,
                &raw mut root,
                &raw mut x,
                &raw mut y,
                &raw mut result.width,
                &raw mut result.height,
                &raw mut border,
                &raw mut depth,
            )
        } == 0
            || depth != 24
        {
            return Err(NativeError::DisplayUnavailable);
        }
        let mut count = -1;
        // SAFETY: library owns returned storage. Immediately wrap it for every
        // exit path, then validate counts before pointer arithmetic/copying.
        let allocation = MonitorAllocation(unsafe {
            XRRGetMonitors(self.display.as_ptr(), self.root, 1, &raw mut count)
        });
        result.count = usize::try_from(count).map_err(|_| NativeError::DisplayUnavailable)?;
        if result.count > MAX_DISPLAYS {
            return Err(NativeError::InvalidConfiguration);
        }
        if result.count != 0 && allocation.0.is_null() {
            return Err(NativeError::DisplayUnavailable);
        }
        for i in 0..result.count {
            // SAFETY: count checked against fixed capacity and the library's
            // successful result; the allocation remains owned throughout copying.
            let native = unsafe { &*allocation.0.add(i) };
            let count =
                usize::try_from(native.noutput).map_err(|_| NativeError::InvalidConfiguration)?;
            if count > MAX_OUTPUTS
                || native.name == 0
                || native.x < 0
                || native.y < 0
                || native.width <= 0
                || native.height <= 0
                || i64::from(native.x) + i64::from(native.width) > i64::from(result.width)
                || i64::from(native.y) + i64::from(native.height) > i64::from(result.height)
                || (count != 0 && native.outputs.is_null())
            {
                return Err(NativeError::InvalidConfiguration);
            }
            let mut monitor = Monitor {
                name: native.name,
                x: native.x,
                y: native.y,
                width: native.width.cast_unsigned(),
                height: native.height.cast_unsigned(),
                outputs: [0; MAX_OUTPUTS],
                count,
            };
            self.limits
                .validate_coded_dimensions(monitor.width, monitor.height)
                .map_err(|_| NativeError::InvalidConfiguration)?;
            for j in 0..count {
                monitor.outputs[j] = unsafe { *native.outputs.add(j) };
                if monitor.outputs[j] == 0 || monitor.outputs[..j].contains(&monitor.outputs[j]) {
                    return Err(NativeError::InvalidConfiguration);
                }
            }
            monitor.outputs[..count].sort_unstable();
            if result.monitors[..i].iter().any(|m| m.name == monitor.name) {
                return Err(NativeError::InvalidConfiguration);
            }
            result.monitors[i] = monitor;
        }
        result.monitors[..result.count].sort_unstable_by_key(|m| m.name);
        Ok(result)
    }
}
impl Drop for X11Inventory {
    fn drop(&mut self) {
        // SAFETY: unique display owner; all query allocations were already freed.
        unsafe { XCloseDisplay(self.display.as_ptr()) };
    }
}
/// A single whole monitor, never the bounding framebuffer around other outputs.
/// Reconfiguration is terminal in this initial profile, including unchanged-size
/// monitor identity replacement. The caller must establish a new selected view.
pub struct X11SelectedCapture {
    inventory: X11Inventory,
    selected: Display,
}
impl X11SelectedCapture {
    pub const fn descriptor(&self) -> Display {
        self.selected
    }
    pub fn revalidate(&mut self) -> Result<(), NativeError> {
        self.inventory.revalidate()
    }
    pub fn snapshot(&mut self) -> Result<BgraFrame, NativeError> {
        self.revalidate()?;
        let d = self.selected;
        let len = super::linux::frame_len(d.pixel_width, d.pixel_height, &self.inventory.limits)?;
        let mut bytes = super::linux::zeroed(len)?;
        // SAFETY: root/connection are retained by this owner; coordinates came
        // from its immutable checked native inventory, never a peer rectangle.
        // The C bridge copies into exactly len initialized bytes and frees XImage.
        let result = super::linux::status(unsafe {
            fr_x11_capture_rectangle(
                self.inventory.display.as_ptr(),
                self.inventory.root,
                d.x,
                d.y,
                c_int::try_from(d.pixel_width).map_err(|_| NativeError::InvalidConfiguration)?,
                c_int::try_from(d.pixel_height).map_err(|_| NativeError::InvalidConfiguration)?,
                bytes.as_mut_ptr(),
                bytes.len(),
            )
        });
        if result.is_err() {
            self.inventory.closed = true;
        }
        result?;
        // A move/removal during XGetImage discards the captured bytes, even when
        // the output returns to identical bounds before this reply arrives.
        self.revalidate()?;
        BgraFrame::new(d.pixel_width, d.pixel_height, bytes, &self.inventory.limits)
    }
}
fn local_selector(name: &str) -> bool {
    if name.len() > 64 {
        return false;
    }
    let Some(tail) = name.strip_prefix(':') else {
        return false;
    };
    let mut parts = tail.split('.');
    let valid = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    parts.next().is_some_and(valid) && parts.next().is_none_or(valid) && parts.next().is_none()
}
