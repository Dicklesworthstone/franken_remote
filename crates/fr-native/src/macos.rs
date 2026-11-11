//! macOS viewer (fr-5a4w): an `AppKit` window whose `CAMetalLayer` shows
//! `VideoToolbox`-decoded HEVC, behind the narrow Objective-C boundary in
//! `macos_viewer.m`. The Rust HEVC guard admits the hvcC record and every
//! access unit before `VideoToolbox` sees a byte; the glue never parses media.
//! Decoded pictures stay in `IOSurface`-backed buffers that Metal samples
//! directly: no CPU readback, conversion or copy.
//!
//! `Window` lives on the main thread (`AppKit`'s rule). `Surface` and
//! `HevcDecoder` move to one media thread.
use crate::NativeError;
use crate::macos_keys;
use core::ffi::{c_char, c_int, c_void};
use core::marker::PhantomData;
use core::ptr::NonNull;
use fr_core::limits::ProtocolLimits;
use fr_media::access_unit::{EncodedAccessUnit, FrameId, FrameKind};
use fr_media::config::{CodecConfiguration, ColorRange};
use fr_media::hevc::HevcGuard;
use fr_media::worker::overlay::Update as CursorUpdate;
use std::time::{Duration, Instant};

pub(crate) use crate::macos_text::Commit as TextCommit;
pub(crate) const TEXT_COMPOSER_FLAG: u32 = 16;
pub(crate) const FILE_SELECTION_FLAG: u32 = 32;

/// A consumed native slot, including a locally refused gesture. A native error
/// is separate because it must fence the original owner before native cleanup.
pub(crate) enum TextTaken {
    Empty,
    Refused { context: u64 },
    Commit(TextCommit),
}

/// Park never runs `AppKit` cleanup; Fenced requires the original owners stopped.
#[derive(Clone, Copy)]
pub(crate) enum TextRetireMode {
    Park,
    Fenced,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextRetirement {
    Parked,
    Veto,
    Clean,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawEvent {
    kind: u32,
    code: u32,
    modifiers: u64,
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
    flags: u32,
    reserved: u32,
    timestamp: f64,
}

unsafe extern "C" {
    fn fr_mac_app_init() -> c_int;
    fn fr_mac_window_box(
        reserved: u32,
        width: *mut u32,
        height: *mut u32,
        scale: *mut u32,
    ) -> c_int;
    fn fr_mac_event_time() -> f64;
    #[allow(clippy::too_many_arguments)]
    fn fr_mac_window_new(
        width: u32,
        height: u32,
        point_width: u32,
        point_height: u32,
        sharing_flags: u32,
        title: *const c_char,
        title_len: usize,
        out: *mut *mut c_void,
    ) -> c_int;
    fn fr_mac_window_poll(
        window: *mut c_void,
        timeout: f64,
        out: *mut RawEvent,
        capacity: usize,
        overflow: *mut c_int,
    ) -> usize;
    fn fr_mac_window_number(window: *mut c_void) -> isize;
    #[allow(clippy::too_many_arguments)]
    fn fr_mac_window_sharing_update(
        window: *mut c_void,
        upload_status: u32,
        upload_done: u64,
        upload_total: u64,
        download_status: u32,
        download_done: u64,
        download_total: u64,
    ) -> c_int;
    fn fr_mac_window_sharing_state(
        window: *mut c_void,
        actions: *mut u32,
        visible: *mut c_int,
    ) -> c_int;
    fn fr_mac_window_display_change_enabled(window: *mut c_void, enabled: c_int) -> c_int;
    fn fr_mac_window_take_display_change(window: *mut c_void, captured: *mut f64) -> c_int;
    fn fr_mac_window_text_update(
        window: *mut c_void,
        context: u64,
        until: u64,
        held_clear: c_int,
    ) -> c_int;
    fn fr_mac_window_text_take(
        window: *mut c_void,
        bytes: *mut u8,
        capacity: usize,
        length: *mut usize,
        context: *mut u64,
        captured: *mut f64,
        until: *mut u64,
    ) -> c_int;
    fn fr_mac_window_text_stopped(window: *mut c_void, fenced: c_int) -> c_int;
    fn fr_mac_window_text_complete(window: *mut c_void, context: u64, admitted: c_int) -> c_int;
    fn fr_mac_window_clipboard_bind(
        window: *mut c_void,
        context: *mut c_void,
        stop: unsafe extern "C" fn(*mut c_void, c_int),
        release: unsafe extern "C" fn(*mut c_void),
    ) -> c_int;
    fn fr_mac_window_clipboard_update(window: *mut c_void, state: u32) -> c_int;
    fn fr_mac_window_clipboard_visible(window: *mut c_void, visible: *mut c_int) -> c_int;
    fn fr_mac_window_file_selection_bind(
        window: *mut c_void,
        context: *mut c_void,
        ready: unsafe extern "C" fn(*mut c_void) -> c_int,
        submit: unsafe extern "C" fn(*mut c_void, *const u8, usize, f64) -> c_int,
        stop: unsafe extern "C" fn(*mut c_void),
        release: unsafe extern "C" fn(*mut c_void),
    ) -> c_int;
    fn fr_mac_window_file_selection_update(window: *mut c_void) -> c_int;
    fn fr_mac_window_state(
        window: *mut c_void,
        width: *mut u32,
        height: *mut u32,
        key: *mut c_int,
        visible: *mut c_int,
        geometry_current: *mut c_int,
    ) -> c_int;
    fn fr_mac_window_resize_bind(
        window: *mut c_void,
        context: *mut c_void,
        changed: unsafe extern "C" fn(*mut c_void, *const crate::macos_resize::Sample),
        release: unsafe extern "C" fn(*mut c_void),
        initial: *mut crate::macos_resize::Sample,
    ) -> c_int;
    fn fr_mac_window_resize_seal(
        window: *mut c_void,
        sample: *mut crate::macos_resize::Sample,
    ) -> c_int;
    fn fr_mac_window_place(window: *mut c_void, x: i32, y: i32) -> c_int;
    fn fr_mac_window_full_screen(window: *mut c_void, enter: c_int, moving: *mut c_int) -> c_int;
    fn fr_mac_window_set_resizable(window: *mut c_void, resizable: c_int) -> c_int;
    fn fr_mac_window_renderer(window: *mut c_void) -> *mut c_void;
    fn fr_mac_surface_offscreen(width: u32, height: u32, out: *mut *mut c_void) -> c_int;
    fn fr_mac_window_free(window: *mut c_void);
    fn fr_mac_renderer_presented(renderer: *mut c_void) -> u64;
    fn fr_mac_renderer_refresh(renderer: *mut c_void) -> c_int;
    #[allow(clippy::too_many_arguments)]
    fn fr_mac_renderer_cursor(
        renderer: *mut c_void,
        kind: c_int,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        hotspot_x: u32,
        hotspot_y: u32,
        rgba: *const u8,
        len: usize,
    ) -> c_int;
    fn fr_mac_renderer_release(renderer: *mut c_void);
    fn fr_mac_decoder_new(
        renderer: *mut c_void,
        vps: *const u8,
        vps_len: usize,
        sps: *const u8,
        sps_len: usize,
        pps: *const u8,
        pps_len: usize,
        width: u32,
        height: u32,
        full_range: c_int,
        out: *mut *mut c_void,
    ) -> c_int;
    fn fr_mac_decoder_hardware(decoder: *mut c_void) -> c_int;
    fn fr_mac_decoder_decode(
        decoder: *mut c_void,
        bytes: *const u8,
        len: usize,
        pts: i64,
        present: c_int,
    ) -> c_int;
    fn fr_mac_decoder_free(decoder: *mut c_void);
}

fn status(code: c_int) -> Result<(), NativeError> {
    match code {
        0 => Ok(()),
        -1 => Err(NativeError::InvalidConfiguration),
        -2 | -6 => Err(NativeError::Unavailable),
        -4 => Err(NativeError::Allocation),
        -5 => Err(NativeError::GeometryChanged),
        -7 => Err(NativeError::Closed),
        _ => Err(NativeError::Codec),
    }
}

/// Make this process a regular `AppKit` application. Main thread only; call
/// once before opening a window.
pub fn initialize_application() -> Result<(), NativeError> {
    // SAFETY: no arguments; the glue refuses any thread but the main one.
    status(unsafe { fr_mac_app_init() })
}

pub use crate::macos_sizing::WindowBox;

/// The largest picture a new viewer window on the main screen shows 1:1:
/// the visible screen below the title bar and `reserved` points of native
/// controls, in the screen's pixels (a Retina screen's backing pixels), with
/// its whole pixels per point. `None` off the main thread or without a usable
/// main screen.
pub fn window_box(reserved: u32) -> Option<WindowBox> {
    let (mut width, mut height, mut scale) = (0, 0, 0);
    // SAFETY: three writable scalars the glue fills synchronously; it refuses
    // a non-main thread before touching AppKit.
    let status =
        unsafe { fr_mac_window_box(reserved, &raw mut width, &raw mut height, &raw mut scale) };
    (status == 0).then_some(WindowBox {
        pixels: (width, height),
        scale,
        reserved,
    })
}

/// Native uptime in the same domain as the original `NSEvent.timestamp`.
/// Sampling this when an event is consumed cannot refresh its capture time.
pub(crate) fn event_time() -> Option<Duration> {
    // SAFETY: no arguments, no retained data or thread-affine state.
    Duration::try_from_secs_f64(unsafe { fr_mac_event_time() }).ok()
}

/// A pointer position relative to the presented picture: (0, 0) is its
/// top-left corner and (1, 1) its bottom-right. Outside the picture (the
/// letterbox bars or beyond the window while dragging) `inside` is false.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PicturePoint {
    pub x: f64,
    pub y: f64,
    pub inside: bool,
    /// Original native event time; never refreshed when the UI queue drains.
    pub captured: Option<Duration>,
}
impl PicturePoint {
    /// The remote pixel under this point, clamped to the picture.
    pub fn pixel(self, width: u32, height: u32) -> (u32, u32) {
        let clamp = |v: f64, n: u32| {
            let max = f64::from(n.saturating_sub(1));
            // Finite, clamped to [0, n - 1] before the conversion.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let p = (v * f64::from(n)).floor().clamp(0.0, max) as u32;
            p
        };
        let x = if self.x.is_finite() { self.x } else { 0.0 };
        let y = if self.y.is_finite() { self.y } else { 0.0 };
        (clamp(x, width), clamp(y, height))
    }
}

/// One local input or window event, with keys already reduced to HID usages.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WindowEvent {
    /// A physical key (HID usage page 0x07) changed state.
    Key {
        usage: u16,
        pressed: bool,
        repeat: bool,
        /// Original native event time; invalid timestamps confer no intent.
        captured: Option<Duration>,
        /// `AppKit`'s device-independent modifier flags at capture time.
        modifiers: u64,
    },
    /// A key with no HID keyboard usage (Fn, vendor keys). Never forwarded.
    UnmappedKey {
        keycode: u16,
    },
    Pointer(PicturePoint),
    /// `AppKit` button number: 0 left, 1 right, 2 middle, then extra buttons.
    Button {
        button: u8,
        pressed: bool,
        at: PicturePoint,
    },
    /// `AppKit` scrolling deltas: points when `precise` (trackpads), else lines.
    Scroll {
        dx: f64,
        dy: f64,
        precise: bool,
        at: PicturePoint,
    },
    /// The user asked to close the window. It stays open until dropped.
    CloseRequested,
    /// Keyboard focus left the window: held remote keys must be released.
    FocusLost,
    FocusGained,
    /// Events arrived faster than they were polled and some were lost.
    Overflow,
}

/// What input and visibility evidence may rely on, sampled on the main thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowState {
    /// The aspect-fit picture in drawable (physical) pixels; (0, 0) before
    /// the first layout.
    pub picture: (u32, u32),
    /// The window receives keyboard input.
    pub key: bool,
    /// Some of the window is on screen: not hidden, minimized or occluded.
    pub visible: bool,
    /// A real picture was submitted at the current drawable geometry. False
    /// until the media owner repaints a changed layout; never a fresh frame
    /// receipt or proof that the compositor finished displaying the picture.
    pub geometry_current: bool,
}
impl WindowState {
    pub(crate) const fn input_ready(self) -> bool {
        self.key
            && self.visible
            && self.geometry_current
            && self.picture.0 > 0
            && self.picture.1 > 0
    }
}

/// The viewer window. Main thread only.
pub struct Window {
    raw: NonNull<c_void>,
    modifiers: Modifiers,
    buffer: Vec<RawEvent>,
    input_attachment: crate::macos_text::Attachment,
    _main_thread: PhantomData<*mut ()>,
}
impl Window {
    /// Opens a window for a `width` x `height` remote picture, sized at the
    /// remote display's logical size `points` (shrunk to fit the screen). A
    /// 2x remote then lands 1:1 on a 2x screen.
    pub fn open(
        width: u32,
        height: u32,
        points: (u32, u32),
        title: &str,
    ) -> Result<Self, NativeError> {
        Self::open_with_sharing(width, height, points, title, 0)
    }
    /// Reserve requested native file controls before creating the viewport.
    pub(crate) fn open_with_sharing(
        width: u32,
        height: u32,
        points: (u32, u32),
        title: &str,
        sharing_flags: u32,
    ) -> Result<Self, NativeError> {
        let mut raw = core::ptr::null_mut();
        // SAFETY: title bytes live for the call and the glue copies them; the
        // out pointer is writable. The glue refuses a non-main thread.
        status(unsafe {
            fr_mac_window_new(
                width,
                height,
                points.0,
                points.1,
                sharing_flags,
                title.as_ptr().cast(),
                title.len(),
                &raw mut raw,
            )
        })?;
        Ok(Self {
            raw: NonNull::new(raw).ok_or(NativeError::Allocation)?,
            modifiers: Modifiers::default(),
            // Drain the glue's entire bounded batch before deciding whether
            // any local control gesture is eligible.
            buffer: vec![RawEvent::default(); 1024],
            input_attachment: crate::macos_text::Attachment::default(),
            _main_thread: PhantomData,
        })
    }
    /// Bind one metadata-only native watcher to this original session. Its
    /// callback fences input before `AppKit` can deliver any post-resize event.
    pub(crate) fn watch_resize(
        &self,
        intent: &std::rc::Rc<crate::macos_resize::Intent>,
        stop: &frd::session_startup::StreamingViewerControl,
    ) -> Result<(), NativeError> {
        let bridge = Box::new(ResizeBridge {
            intent: intent.clone(),
            stop: stop.clone(),
        });
        let context = Box::into_raw(bridge).cast();
        let mut initial = crate::macos_resize::Sample::default();
        // SAFETY: main-thread live window and writable scalar output. Native
        // takes the Box only on success and clears callbacks before release.
        let result = status(unsafe {
            fr_mac_window_resize_bind(
                self.raw.as_ptr(),
                context,
                resize_changed,
                resize_release,
                &raw mut initial,
            )
        });
        if let Err(error) = result {
            // SAFETY: failed binding did not retain the unique context.
            drop(unsafe { Box::from_raw(context.cast::<ResizeBridge>()) });
            intent.cancel();
            return Err(error);
        }
        if !intent.arm(initial) {
            intent.cancel();
            stop.stop();
            return Err(NativeError::GeometryChanged);
        }
        if let Err(error) = self.set_resizable(true) {
            intent.cancel();
            stop.stop();
            return Err(error);
        }
        Ok(())
    }

    /// Called after the original stop and a complete bounded event drain.
    pub(crate) fn seal_resize(&self) -> Result<crate::macos_resize::Sample, NativeError> {
        let mut sample = crate::macos_resize::Sample::default();
        // SAFETY: live main-thread owner and one writable scalar receipt.
        status(unsafe { fr_mac_window_resize_seal(self.raw.as_ptr(), &raw mut sample) })?;
        Ok(sample)
    }

    /// Whether `AppKit` is still animating this window into or out of full
    /// screen. Its final size is only known when this is false.
    pub(crate) fn full_screen_moving(&self) -> Result<bool, NativeError> {
        let mut moving = 0;
        // SAFETY: live main-thread window and one writable scalar.
        status(unsafe { fr_mac_window_full_screen(self.raw.as_ptr(), 0, &raw mut moving) })?;
        Ok(moving != 0)
    }

    /// Ask `AppKit` to take this NEW window into full screen, as the window it
    /// replaces was. A window already in full screen is left as it is.
    pub(crate) fn enter_full_screen(&self) -> Result<(), NativeError> {
        let mut moving = 0;
        // SAFETY: live main-thread window and one writable scalar.
        status(unsafe { fr_mac_window_full_screen(self.raw.as_ptr(), 1, &raw mut moving) })
    }

    pub(crate) fn place(
        &self,
        placement: crate::macos_resize::Placement,
    ) -> Result<(), NativeError> {
        // SAFETY: checked local point scalars; native also checks a currently
        // visible screen before moving this newly constructed window.
        status(unsafe {
            fr_mac_window_place(self.raw.as_ptr(), placement.origin.0, placement.origin.1)
        })
    }

    pub(crate) fn sharing_update(&self, view: sharing::View) -> Result<(), NativeError> {
        // SAFETY: live main-thread window, bounded status enums and scalar counts.
        status(unsafe {
            fr_mac_window_sharing_update(
                self.raw.as_ptr(),
                view.upload.state as u32,
                view.upload.done,
                view.upload.total,
                view.download.state as u32,
                view.download.done,
                view.download.total,
            )
        })
    }
    pub(crate) fn sharing_state(&self) -> Result<sharing::Sample, NativeError> {
        let (mut actions, mut visible) = (0, 0);
        // SAFETY: live main-thread window; both out parameters are writable.
        status(unsafe {
            fr_mac_window_sharing_state(self.raw.as_ptr(), &raw mut actions, &raw mut visible)
        })?;
        Ok(sharing::Sample {
            actions,
            visible: visible != 0,
        })
    }
    pub(crate) fn display_change_enabled(&self, enabled: bool) -> Result<(), NativeError> {
        // SAFETY: live window on its owning main thread; a bounded local flag.
        status(unsafe {
            fr_mac_window_display_change_enabled(self.raw.as_ptr(), c_int::from(enabled))
        })
    }
    pub(crate) fn take_display_change(&self) -> Result<Option<Duration>, NativeError> {
        let mut captured = 0.0;
        // SAFETY: live main-thread window and one writable scalar output.
        status(unsafe { fr_mac_window_take_display_change(self.raw.as_ptr(), &raw mut captured) })?;
        if captured == 0.0 {
            return Ok(None);
        }
        Duration::try_from_secs_f64(captured)
            .map(Some)
            .map_err(|_| NativeError::Unavailable)
    }
    /// One original producer per window; a new grant cannot reuse native serials.
    pub(crate) fn claim_input(&self) -> Result<(), NativeError> {
        self.input_attachment
            .claim()
            .map_err(|()| NativeError::Unavailable)
    }
    /// Park only retires local state and probes for a veto. Fenced permits
    /// actual native cleanup; an already-observed negative is not cleanup failure.
    pub(crate) fn text_stopped(&self, mode: TextRetireMode) -> Result<TextRetirement, NativeError> {
        let fenced = match mode {
            TextRetireMode::Park => 0,
            TextRetireMode::Fenced => 1,
        };
        // SAFETY: live main-thread window; Fenced requires stopped owners.
        let result = unsafe { fr_mac_window_text_stopped(self.raw.as_ptr(), fenced) };
        match (mode, result) {
            (TextRetireMode::Park, 0) => Ok(TextRetirement::Parked),
            (TextRetireMode::Park, 1) => Ok(TextRetirement::Veto),
            (TextRetireMode::Fenced, 0 | 1) => Ok(TextRetirement::Clean),
            (_, result) => status(result).and(Err(NativeError::Unavailable)),
        }
    }
    /// Publish only the original session's opaque text context. Losing or
    /// replacing it clears native preedit, draft and an unsent Send. Repeated
    /// publication of the same context preserves local editing. The opaque
    /// client-clock `until` is frozen by native Send, never compared to uptime.
    /// `held_clear` is a negative gate, never permission to release remote input.
    pub(crate) fn text_update(
        &self,
        context: Option<u64>,
        until: u64,
        held_clear: bool,
    ) -> Result<(), NativeError> {
        if context == Some(0) || (context.is_none() && until != 0) {
            return Err(NativeError::InvalidConfiguration);
        }
        // SAFETY: live main-thread window and scalar, content-free metadata.
        status(unsafe {
            fr_mac_window_text_update(
                self.raw.as_ptr(),
                context.unwrap_or(0),
                until,
                c_int::from(held_clear),
            )
        })
    }
    /// Consume at most one original local Send. Call only after validating the
    /// whole native batch: close, escape, focus, geometry and overflow win.
    /// The native editor stays exclusive through the pump that captures Send,
    /// so no later physical action can overtake this side-slot commit.
    pub(crate) fn take_text(&self) -> Result<TextTaken, NativeError> {
        let mut bytes = [0; fr_core::input::MAX_COMMITTED_TEXT_BYTES];
        let (mut length, mut context, mut captured, mut until) = (0, 0, 0.0, 0);
        // SAFETY: live main-thread window; exactly capacity writable bytes and
        // valid scalar outputs. The glue clears its one slot before returning.
        let taken = status(unsafe {
            fr_mac_window_text_take(
                self.raw.as_ptr(),
                bytes.as_mut_ptr(),
                bytes.len(),
                &raw mut length,
                &raw mut context,
                &raw mut captured,
                &raw mut until,
            )
        });
        let result = (|| {
            taken?;
            if length == 0 {
                return if context == 0 && captured == 0.0 && until == 0 {
                    Ok(TextTaken::Empty)
                } else {
                    Err(NativeError::Unavailable)
                };
            }
            if context == 0 || until == 0 {
                return Err(NativeError::Unavailable);
            }
            let value = bytes.get(..length).ok_or(NativeError::Unavailable)?;
            let now = event_time().ok_or(NativeError::Unavailable)?;
            Ok(
                match TextCommit::from_native(context, until, captured, value, now) {
                    Ok(commit) => TextTaken::Commit(commit),
                    Err(_) => TextTaken::Refused { context },
                },
            )
        })();
        // Best-effort local cleanup, not a claim about compiler or AppKit copies.
        bytes.fill(0);
        result
    }
    /// Complete that one consumed Send before another native pump. Admission
    /// clears the draft; refusal preserves it only under the same context and
    /// requires a fresh explicit Send. Neither outcome claims OS submission.
    pub(crate) fn text_complete(&self, context: u64, admitted: bool) -> Result<(), NativeError> {
        if context == 0 {
            return Err(NativeError::InvalidConfiguration);
        }
        // SAFETY: live main-thread window and content-free completion metadata.
        status(unsafe {
            fr_mac_window_text_complete(self.raw.as_ptr(), context, c_int::from(admitted))
        })
    }
    pub(crate) fn file_selection_bind(
        &self,
        ingress: crate::macos_file_drop::Ingress,
    ) -> Result<(), NativeError> {
        let context = Box::into_raw(Box::new(FileSelectionBridge(ingress)));
        // SAFETY: the native main-thread window adopts the unique Box only on
        // success, clearing every callback before its sole release callback.
        let result = status(unsafe {
            fr_mac_window_file_selection_bind(
                self.raw.as_ptr(),
                context.cast(),
                file_selection_ready,
                file_selection_submit,
                file_selection_stop,
                file_selection_release,
            )
        });
        if result.is_err() {
            // SAFETY: failed native binding retained no pointer or callback.
            drop(unsafe { Box::from_raw(context) });
        }
        result
    }
    pub(crate) fn file_selection_update(&self) -> Result<(), NativeError> {
        // SAFETY: a live window on the same owning main thread, no out pointers.
        status(unsafe { fr_mac_window_file_selection_update(self.raw.as_ptr()) })
    }

    /// Bind the original local Off owner before any clipboard setup can run.
    pub(crate) fn clipboard_bind(&self, privacy: clipboard::Privacy) -> Result<(), NativeError> {
        let context = Box::into_raw(Box::new(ClipboardSwitch(privacy)));
        // SAFETY: the main-thread window adopts this unique Box only on success;
        // callbacks are main-thread only and are cleared before freeing it.
        let result = status(unsafe {
            fr_mac_window_clipboard_bind(
                self.raw.as_ptr(),
                context.cast(),
                clipboard_stop,
                clipboard_release,
            )
        });
        if result.is_err() {
            // SAFETY: failed binding did not adopt or retain the unique Box.
            drop(unsafe { Box::from_raw(context) });
        }
        result
    }
    pub(crate) fn clipboard_update(&self, state: u32) -> Result<(), NativeError> {
        // SAFETY: live window on its owning main thread; glue validates the enum.
        status(unsafe { fr_mac_window_clipboard_update(self.raw.as_ptr(), state) })
    }
    pub(crate) fn clipboard_visible(&self) -> Result<bool, NativeError> {
        let mut visible = 0;
        // SAFETY: live main-thread window and one writable scalar output.
        status(unsafe { fr_mac_window_clipboard_visible(self.raw.as_ptr(), &raw mut visible) })?;
        Ok(visible != 0)
    }
    /// Current picture size and input-relevant window state.
    pub fn state(&self) -> Result<WindowState, NativeError> {
        let (mut width, mut height, mut key, mut visible, mut geometry_current) = (0, 0, 0, 0, 0);
        // SAFETY: live handle on its owning thread; all out pointers writable.
        status(unsafe {
            fr_mac_window_state(
                self.raw.as_ptr(),
                &raw mut width,
                &raw mut height,
                &raw mut key,
                &raw mut visible,
                &raw mut geometry_current,
            )
        })?;
        Ok(WindowState {
            picture: (width, height),
            key: key != 0,
            visible: visible != 0,
            geometry_current: geometry_current != 0,
        })
    }
    /// Fix (false) or free (true) the window's size.
    pub fn set_resizable(&self, resizable: bool) -> Result<(), NativeError> {
        // SAFETY: live handle on its owning thread.
        status(unsafe { fr_mac_window_set_resizable(self.raw.as_ptr(), c_int::from(resizable)) })
    }
    /// The `CGWindowID` (for `screencapture -l`).
    pub fn number(&self) -> isize {
        // SAFETY: live window handle on its owning thread.
        unsafe { fr_mac_window_number(self.raw.as_ptr()) }
    }
    /// A presentation handle for the media thread.
    pub fn surface(&self) -> Surface {
        // SAFETY: live window handle; the glue returns a +1 renderer reference.
        let raw = unsafe { fr_mac_window_renderer(self.raw.as_ptr()) };
        Surface {
            raw: NonNull::new(raw).expect("a live window always has a renderer"),
        }
    }
    /// Runs the `AppKit` event loop for up to `timeout` (returning at the first
    /// event) and appends what arrived.
    pub fn poll(&mut self, timeout: Duration, events: &mut Vec<WindowEvent>) {
        let mut overflow = 0;
        // SAFETY: the buffer is writable for its length; live handle, main thread.
        let n = unsafe {
            fr_mac_window_poll(
                self.raw.as_ptr(),
                timeout.as_secs_f64(),
                self.buffer.as_mut_ptr(),
                self.buffer.len(),
                &raw mut overflow,
            )
        };
        for i in 0..n.min(self.buffer.len()) {
            let raw = self.buffer[i];
            self.translate(raw, events);
        }
        if overflow != 0 {
            events.push(WindowEvent::Overflow);
        }
    }
    fn translate(&mut self, e: RawEvent, events: &mut Vec<WindowEvent>) {
        let at = PicturePoint {
            x: e.x,
            y: e.y,
            inside: e.flags & 1 != 0,
            captured: Duration::try_from_secs_f64(e.timestamp).ok(),
        };
        let keycode = u16::try_from(e.code).unwrap_or(u16::MAX);
        match e.kind {
            1 | 2 => events.push(match macos_keys::usage(keycode) {
                Some(usage) => WindowEvent::Key {
                    usage,
                    pressed: e.kind == 1,
                    repeat: e.kind == 1 && e.flags & 1 != 0,
                    captured: Duration::try_from_secs_f64(e.timestamp).ok(),
                    modifiers: e.modifiers,
                },
                None => WindowEvent::UnmappedKey { keycode },
            }),
            3 => self.modifiers.change(
                keycode,
                e.modifiers,
                Duration::try_from_secs_f64(e.timestamp).ok(),
                events,
            ),
            4 => events.push(WindowEvent::Pointer(at)),
            5 | 6 => {
                if let Ok(button) = u8::try_from(e.code) {
                    events.push(WindowEvent::Button {
                        button,
                        pressed: e.kind == 5,
                        at,
                    });
                }
            }
            7 => events.push(WindowEvent::Scroll {
                dx: e.dx,
                dy: e.dy,
                precise: e.flags & 2 != 0,
                at,
            }),
            8 => events.push(WindowEvent::CloseRequested),
            9 => {
                // Released remotely by the session; local tracking starts over.
                self.modifiers = Modifiers::default();
                events.push(WindowEvent::FocusLost);
            }
            10 => events.push(WindowEvent::FocusGained),
            _ => {}
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: unique owner of the +1 window reference, on the main thread.
        unsafe { fr_mac_window_free(self.raw.as_ptr()) };
    }
}

// Main-thread-only and owned by one exact Window. The policy uses Cell, so
// these native callbacks perform no session borrow, lock, native work or I/O.
struct ResizeBridge {
    intent: std::rc::Rc<crate::macos_resize::Intent>,
    stop: frd::session_startup::StreamingViewerControl,
}
impl Drop for ResizeBridge {
    fn drop(&mut self) {
        self.stop.stop();
    }
}
unsafe extern "C" fn resize_changed(
    context: *mut c_void,
    sample: *const crate::macos_resize::Sample,
) {
    if context.is_null() {
        return;
    }
    // SAFETY: the original window retains this Box until it clears every
    // callback, and the native stack sample lives for this same-thread call.
    let bridge = unsafe { &*context.cast::<ResizeBridge>() };
    if sample.is_null() {
        bridge.intent.cancel();
    } else {
        // SAFETY: checked non-null above; native passes its own stack sample,
        // valid for this same-thread call, and Sample is plain scalar data.
        let sample = unsafe { *sample };
        bridge.intent.observe(sample, bridge.stop.is_stopped());
    }
    // Recording a causal receipt is not authority. Stop is terminal even when
    // the sample was malformed or an earlier local failure already vetoed it.
    bridge.stop.stop();
}
unsafe extern "C" fn resize_release(context: *mut c_void) {
    if !context.is_null() {
        // SAFETY: exactly once after native clears the original callback slots.
        drop(unsafe { Box::from_raw(context.cast::<ResizeBridge>()) });
    }
}

/// The native Off button borrows this exact original privacy owner. It never
/// calls into the session future or waits for clipboard/native cleanup.
struct ClipboardSwitch(clipboard::Privacy);
impl Drop for ClipboardSwitch {
    fn drop(&mut self) {
        self.0.stop(false);
    }
}
unsafe extern "C" fn clipboard_stop(context: *mut c_void, user_off: c_int) {
    if context.is_null() {
        return;
    }
    // SAFETY: glue retains the Box until it clears both callbacks on the same
    // main thread; this callback only borrows it and performs no native work.
    let switch = unsafe { &*context.cast::<ClipboardSwitch>() };
    switch.0.stop(user_off != 0);
}
unsafe extern "C" fn clipboard_release(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: exactly once after the glue has cleared its context/callbacks.
    drop(unsafe { Box::from_raw(context.cast::<ClipboardSwitch>()) });
}

struct FileSelectionBridge(crate::macos_file_drop::Ingress);
impl Drop for FileSelectionBridge {
    fn drop(&mut self) {
        self.0.stop();
    }
}
unsafe extern "C" fn file_selection_ready(context: *mut c_void) -> c_int {
    if context.is_null() {
        return 0;
    }
    // SAFETY: same-thread native callbacks borrow the Box until native clears
    // all callback slots before release. No session or AppKit reentry occurs.
    let bridge = unsafe { &*context.cast::<FileSelectionBridge>() };
    c_int::from(bridge.0.ready())
}
unsafe extern "C" fn file_selection_submit(
    context: *mut c_void,
    bytes: *const u8,
    length: usize,
    age: f64,
) -> c_int {
    if context.is_null()
        || bytes.is_null()
        || length == 0
        || length > crate::file_drop::MAX_BYTES
        || !(0.0..crate::file_drop::MAX_AGE.as_secs_f64()).contains(&age)
    {
        return 0;
    }
    let Some(captured) = Instant::now().checked_sub(Duration::from_secs_f64(age)) else {
        return 0;
    };
    // SAFETY: native lends its initialized bounded stack buffer for this call;
    // neither the borrowed bytes nor a native pointer escapes into the worker.
    let bytes = unsafe { std::slice::from_raw_parts(bytes, length) };
    // SAFETY: same original callback Box, retained until native clears slots.
    let bridge = unsafe { &*context.cast::<FileSelectionBridge>() };
    c_int::from(bridge.0.submit(bytes, captured))
}
unsafe extern "C" fn file_selection_stop(context: *mut c_void) {
    if !context.is_null() {
        // SAFETY: native borrows its original Box; stop is idempotent and has
        // no filesystem, AppKit or main-thread session operations.
        unsafe { &*context.cast::<FileSelectionBridge>() }.0.stop();
    }
}
unsafe extern "C" fn file_selection_release(context: *mut c_void) {
    if !context.is_null() {
        // SAFETY: exactly once, after native clears every callback and pointer.
        drop(unsafe { Box::from_raw(context.cast::<FileSelectionBridge>()) });
    }
}

/// Held modifier keys, for turning `FlagsChanged` into presses and releases.
#[derive(Clone, Copy, Debug, Default)]
struct Modifiers {
    held: u8,
}
impl Modifiers {
    /// `FlagsChanged` names the key but not the direction: read it from the
    /// side-specific device bit, falling back to the shared flag for
    /// keyboards that report no device bits. Duplicates are dropped.
    fn change(
        &mut self,
        keycode: u16,
        flags: u64,
        captured: Option<Duration>,
        events: &mut Vec<WindowEvent>,
    ) {
        if keycode == macos_keys::CAPS_LOCK {
            // Each change is one press of a toggling key.
            for pressed in [true, false] {
                events.push(WindowEvent::Key {
                    usage: 0x39,
                    pressed,
                    repeat: false,
                    captured,
                    modifiers: flags,
                });
            }
            return;
        }
        let Some(m) = macos_keys::modifier(keycode) else {
            events.push(WindowEvent::UnmappedKey { keycode });
            return;
        };
        let pressed = if flags & macos_keys::DEVICE_MASKS != 0 || flags & m.flag == 0 {
            flags & m.device_mask != 0
        } else {
            flags & m.flag != 0
        };
        let bit = 1_u8 << (m.usage - 0xe0);
        if pressed == (self.held & bit != 0) {
            return;
        }
        self.held ^= bit;
        events.push(WindowEvent::Key {
            usage: m.usage,
            pressed,
            repeat: false,
            captured,
            modifiers: flags,
        });
    }
}

/// The window's Metal presenter, shared with the media thread.
pub struct Surface {
    raw: NonNull<c_void>,
}
// SAFETY: the renderer serializes native media calls on its render lock and
// publishes only plain geometry/counters through a separate short lock. It
// holds no thread-affine AppKit state; CAMetalLayer drawables are thread-safe.
unsafe impl Send for Surface {}
impl Surface {
    /// A presenter with no window, drawing into a detached layer of
    /// `width` x `height` pixels. Any thread: decode tests and benchmarks.
    pub fn offscreen(width: u32, height: u32) -> Result<Self, NativeError> {
        let mut raw = core::ptr::null_mut();
        // SAFETY: scalar sizes and a writable out pointer.
        status(unsafe { fr_mac_surface_offscreen(width, height, &raw mut raw) })?;
        Ok(Self {
            raw: NonNull::new(raw).ok_or(NativeError::Allocation)?,
        })
    }
    /// Pictures submitted to the display (presentation requests, not a
    /// visibility witness).
    pub fn presented(&self) -> u64 {
        // SAFETY: live renderer reference.
        unsafe { fr_mac_renderer_presented(self.raw.as_ptr()) }
    }
    /// Repaint a pending resize or expose on the owning media thread. This
    /// submits the retained picture without creating a new frame receipt or
    /// increasing `presented`. The main thread only publishes redraw intent.
    pub fn refresh(&self) -> Result<(), NativeError> {
        // SAFETY: live renderer reference; the glue serializes media calls.
        status(unsafe { fr_mac_renderer_refresh(self.raw.as_ptr()) })
    }
    /// Install the single client-rendered remote cursor (hotspot in decoded
    /// picture pixels; the host capture excludes the pointer) and redraw the
    /// retained picture with it. `Move` needs an earlier `Shape`; `Hidden`
    /// keeps the shape. Invalid input changes nothing; a native draw failure
    /// ends the media operation without confirming display. This is not a
    /// decode, presentation receipt or visibility proof.
    pub fn cursor(&self, update: &CursorUpdate<'_>) -> Result<(), NativeError> {
        let (kind, x, y, shape) = match *update {
            CursorUpdate::Hidden => (0, 0, 0, None),
            CursorUpdate::Move { x, y } => (1, x, y, None),
            CursorUpdate::Shape {
                x,
                y,
                width,
                height,
                hotspot_x,
                hotspot_y,
                rgba,
            } => {
                fr_media::cursor::check_geometry(width, height, hotspot_x, hotspot_y, rgba.len())
                    .map_err(|_| NativeError::InvalidConfiguration)?;
                (2, x, y, Some((width, height, hotspot_x, hotspot_y, rgba)))
            }
        };
        let (width, height, hotspot_x, hotspot_y, rgba) = shape.unwrap_or((0, 0, 0, 0, &[]));
        // SAFETY: live renderer reference; the glue copies the checked shape
        // into a Metal texture before returning.
        status(unsafe {
            fr_mac_renderer_cursor(
                self.raw.as_ptr(),
                kind,
                x,
                y,
                u32::from(width),
                u32::from(height),
                u32::from(hotspot_x),
                u32::from(hotspot_y),
                rgba.as_ptr(),
                rgba.len(),
            )
        })
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: releases exactly the +1 reference this owner holds.
        unsafe { fr_mac_renderer_release(self.raw.as_ptr()) };
    }
}

/// The VPS, SPS and PPS of a canonical record (`HevcGuard::decoder_record`
/// layout: 23-byte header, then three single-NAL arrays). Call only on a
/// record the guard admitted.
fn parameter_sets(record: &[u8]) -> Option<[&[u8]; 3]> {
    let mut offset: usize = 23;
    let mut sets: [&[u8]; 3] = [&[]; 3];
    for (slot, kind) in sets.iter_mut().zip(32_u8..=34) {
        let header = record.get(offset..offset.checked_add(5)?)?;
        if header[..3] != [0x80 | kind, 0, 1] {
            return None;
        }
        let length = usize::from(u16::from_be_bytes([header[3], header[4]]));
        offset += 5;
        *slot = record.get(offset..offset.checked_add(length)?)?;
        offset += length;
    }
    (offset == record.len()).then_some(sets)
}

/// `VideoToolbox` HEVC decode presenting straight to a `Surface`.
pub struct HevcDecoder {
    raw: NonNull<c_void>,
    surface: Surface,
    config: CodecConfiguration,
    limits: ProtocolLimits,
    admission: HevcGuard,
    last: Option<FrameId>,
    closed: bool,
}
// SAFETY: the VideoToolbox session is used by one thread at a time (`&mut
// self`); its output callback runs synchronously inside decode.
unsafe impl Send for HevcDecoder {}
impl HevcDecoder {
    /// Admits `record` with the HEVC guard, then configures `VideoToolbox` from
    /// its parameter sets. A refused record never reaches `VideoToolbox`.
    pub fn new(
        surface: Surface,
        config: CodecConfiguration,
        limits: ProtocolLimits,
        record: &[u8],
    ) -> Result<Self, NativeError> {
        let admission = HevcGuard::from_decoder_record(config, limits, 4, record)
            .map_err(|_| NativeError::InvalidConfiguration)?;
        let [vps, sps, pps] = parameter_sets(record).ok_or(NativeError::InvalidConfiguration)?;
        let g = config.geometry();
        let full_range = c_int::from(matches!(config.color().range, ColorRange::Full));
        let mut raw = core::ptr::null_mut();
        // SAFETY: admitted parameter-set slices live for the call and
        // CoreMedia copies them; renderer and out pointers are valid.
        status(unsafe {
            fr_mac_decoder_new(
                surface.raw.as_ptr(),
                vps.as_ptr(),
                vps.len(),
                sps.as_ptr(),
                sps.len(),
                pps.as_ptr(),
                pps.len(),
                g.crop_width(),
                g.crop_height(),
                full_range,
                &raw mut raw,
            )
        })?;
        Ok(Self {
            raw: NonNull::new(raw).ok_or(NativeError::Allocation)?,
            surface,
            config,
            limits,
            admission,
            last: None,
            closed: false,
        })
    }
    /// Whether `VideoToolbox` chose a hardware decoder, when it says.
    pub fn hardware(&self) -> Option<bool> {
        // SAFETY: live decoder handle owned by this thread.
        match unsafe { fr_mac_decoder_hardware(self.raw.as_ptr()) } {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    pub fn surface(&self) -> &Surface {
        &self.surface
    }
    /// Admits, decodes and presents one access unit. Admission and decode are
    /// one transaction: a refused or failed unit leaves the guard unchanged.
    /// A decode failure closes the decoder; recovery is a new IDR on a new one.
    pub fn submit(&mut self, unit: &EncodedAccessUnit) -> Result<(), NativeError> {
        self.submit_unit(unit, true)
    }
    /// `submit`, drawing only when `present`: a late picture still decodes
    /// so the pictures that reference it stay decodable.
    pub fn submit_unit(
        &mut self,
        unit: &EncodedAccessUnit,
        present: bool,
    ) -> Result<(), NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        if unit.config_generation() != self.config.generation()
            || unit.frame().as_raw() > i64::MAX as u64
            || self.last.is_some_and(|n| unit.frame() <= n)
        {
            return Err(NativeError::StaleGeneration);
        }
        self.limits
            .validate_access_unit_len(unit.bytes().len())
            .map_err(|_| NativeError::UnsupportedBitstream)?;
        let idr = unit.is_idr();
        if !idr
            && unit.kind()
                != (FrameKind::Predicted {
                    references: self.last.ok_or(NativeError::UnsupportedBitstream)?,
                })
        {
            return Err(NativeError::UnsupportedBitstream);
        }
        let mut admission = self.admission.clone();
        // Checks every NAL against the frozen parameter sets, then strips the
        // in-band copies: VideoToolbox decodes against the configured format.
        let sample = admission
            .prepare_hvc1(unit.bytes(), idr)
            .map_err(|_| NativeError::UnsupportedBitstream)?;
        let pts = i64::try_from(unit.capture_micros()).unwrap_or(i64::MAX);
        let bytes = sample.bytes();
        // SAFETY: the admitted sample lives for this synchronous call and the
        // glue copies it into a CoreMedia block buffer before decoding.
        if let Err(error) = status(unsafe {
            fr_mac_decoder_decode(
                self.raw.as_ptr(),
                bytes.as_ptr(),
                bytes.len(),
                pts,
                c_int::from(present),
            )
        }) {
            self.closed = true;
            return Err(error);
        }
        self.admission = admission;
        self.last = Some(unit.frame());
        Ok(())
    }
}
impl Drop for HevcDecoder {
    fn drop(&mut self) {
        // SAFETY: unique owner; invalidates the session before releasing it.
        unsafe { fr_mac_decoder_free(self.raw.as_ptr()) };
    }
}

#[path = "macos/cadence.rs"]
pub mod cadence;
mod clipboard;
#[path = "macos/file_picker.rs"]
pub mod file_picker;
#[path = "macos/files.rs"]
pub mod files;
#[path = "macos/picker.rs"]
pub mod picker;
#[path = "macos/sharing.rs"]
mod sharing;

// The frd session on top of this window and decoder (needs the CLI's frd).
#[cfg(feature = "cli")]
#[path = "macos/session.rs"]
pub mod session;

#[cfg(test)]
#[path = "macos/tests.rs"]
mod tests;
