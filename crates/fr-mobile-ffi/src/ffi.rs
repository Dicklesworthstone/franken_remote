//! The C ABI in `include/fr_mobile.h`. Every function checks its pointers,
//! never panics across the boundary on bad input, and never blocks the
//! caller's thread on the network. Strings cross as NUL-terminated UTF-8;
//! copies out are bounded by the caller's buffer and always NUL-terminated.
//!
//! Sessions cross as generation-checked handles (ADR 0003): a 64-bit value
//! naming a registry slot and the generation it was issued in. A handle used
//! after `fr_session_free`, a double free, or a made-up value finds no
//! session and returns `FR_STALE` (or nothing); it never reaches freed
//! memory. Any thread may call any function, concurrently.
use crate::{
    directory,
    session::{Config, InputResult, Session},
    video::FrameInfo,
};
use core::ffi::{c_char, c_void};
use fr_tailnet::{HostName, HostPin};
use std::{
    ffi::{CStr, CString},
    str::FromStr,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
};

pub const FR_OK: i32 = 0;
pub const FR_PENDING: i32 = 1;
pub const FR_INVALID: i32 = -1;
pub const FR_NOT_CONTROLLED: i32 = -2;
pub const FR_OUTSIDE: i32 = -3;
pub const FR_UNSUPPORTED: i32 = -4;
pub const FR_CONTROL_LOST: i32 = -5;
pub const FR_CLOSED: i32 = -6;
pub const FR_BUSY: i32 = -7;
pub const FR_STALE: i32 = -8;

/// Most sessions alive at once (the app shows one; a closing one may
/// overlap its successor).
pub const MAX_SESSIONS: usize = 4;

/// A session handle: slot index + 1 in the low half, generation in the
/// high half. Zero is never issued.
pub type FrHandle = u64;

struct Slot {
    generation: u32,
    session: Option<Arc<Session>>,
}
static REGISTRY: Mutex<Vec<Slot>> = Mutex::new(Vec::new());

fn register(session: Session) -> Option<FrHandle> {
    let mut slots = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    let index = match slots.iter().position(|slot| slot.session.is_none()) {
        Some(index) => index,
        None if slots.len() < MAX_SESSIONS => {
            slots.push(Slot {
                generation: 0,
                session: None,
            });
            slots.len() - 1
        }
        None => return None,
    };
    let slot = &mut slots[index];
    // A slot's generation never repeats until 2^32 reuses of that slot.
    slot.generation = slot.generation.wrapping_add(1).max(1);
    slot.session = Some(Arc::new(session));
    Some((u64::from(slot.generation) << 32) | (u64::try_from(index).ok()? + 1))
}
fn split(handle: FrHandle) -> Option<(usize, u32)> {
    let index = usize::try_from(handle & 0xffff_ffff).ok()?.checked_sub(1)?;
    let generation = u32::try_from(handle >> 32).ok()?;
    Some((index, generation))
}
fn lookup(handle: FrHandle) -> Option<Arc<Session>> {
    let (index, generation) = split(handle)?;
    let slots = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    let slot = slots.get(index)?;
    (slot.generation == generation)
        .then(|| slot.session.clone())
        .flatten()
}
fn unregister(handle: FrHandle) -> Option<Arc<Session>> {
    let (index, generation) = split(handle)?;
    let mut slots = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    let slot = slots.get_mut(index)?;
    if slot.generation != generation {
        return None;
    }
    slot.session.take()
}

#[repr(C)]
pub struct FrConfig {
    /// NUL-terminated `MagicDNS` name, `<machine>.<tailnet>.ts.net`.
    pub host: *const c_char,
    pub port: u16,
    /// Request control once the first picture is on screen (still requires
    /// `fr_session_request_control`); false opens a view-only session.
    pub control: bool,
    /// Dial the pin's own addresses instead of resolving `host`: for a
    /// machine listed by a host's tailnet directory. Requires `pin`.
    pub direct: bool,
    /// NUL-terminated pin from an earlier connection to the same name, or
    /// NULL for a first connection.
    pub pin: *const c_char,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FrFrameInfo {
    pub sequence: u64,
    pub frame: u64,
    pub width: u32,
    pub height: u32,
    /// 1 for full-range NV12, 0 for video range.
    pub full_range: u32,
    pub reserved: u32,
}
impl From<FrameInfo> for FrFrameInfo {
    fn from(info: FrameInfo) -> Self {
        Self {
            sequence: info.sequence,
            frame: info.frame,
            width: info.width,
            height: info.height,
            full_range: u32::from(info.full_range),
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FrStats {
    pub presented: u64,
    pub drawn_frame: u64,
    pub decoded: u64,
    pub decode_micros: u64,
    pub first_view_micros: u64,
    /// 1 hardware, 0 software, -1 unknown.
    pub hardware: i32,
    /// 1 controlled now, 0 not.
    pub controlled: u32,
    /// The `FR_END_*` code once ended, else `u32::MAX`.
    pub end: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FrCursor {
    pub generation: u64,
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub visible: u32,
    pub reserved: u32,
}

const fn code(result: InputResult) -> i32 {
    match result {
        InputResult::Sent => FR_OK,
        InputResult::Pending => FR_PENDING,
        InputResult::NotControlled => FR_NOT_CONTROLLED,
        InputResult::Outside => FR_OUTSIDE,
        InputResult::Unsupported => FR_UNSUPPORTED,
        InputResult::Invalid => FR_INVALID,
        InputResult::ControlLost => FR_CONTROL_LOST,
        InputResult::Busy => FR_BUSY,
    }
}

/// # Safety
/// `text` is NULL or a NUL-terminated string valid for reads.
unsafe fn text<'a>(text: *const c_char) -> Option<&'a str> {
    if text.is_null() {
        return None;
    }
    // SAFETY: non-null and NUL-terminated per this function's contract.
    unsafe { CStr::from_ptr(text) }.to_str().ok()
}

/// Copy `value` into `buffer` (NUL-terminated, truncated to fit) and
/// return its full length in bytes without the NUL, so a caller can retry
/// with a larger buffer.
///
/// # Safety
/// `buffer` is NULL or writable for `capacity` bytes.
unsafe fn copy_out(value: &[u8], buffer: *mut c_char, capacity: usize) -> usize {
    if !buffer.is_null() && capacity > 0 {
        let n = value.len().min(capacity - 1);
        // SAFETY: `buffer` is writable for `capacity` > n bytes and does not
        // overlap `value` (Rust-owned memory).
        unsafe {
            core::ptr::copy_nonoverlapping(value.as_ptr().cast::<c_char>(), buffer, n);
            buffer.add(n).write(0);
        }
    }
    value.len()
}

/// Whether `name` is a host name the app accepts (`<machine>.<tailnet>.ts.net`;
/// ASCII letters in either case, digits and inner hyphens). Resolution is
/// not attempted.
///
/// # Safety
/// `name` is NULL or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_host_name_valid(name: *const c_char) -> bool {
    // SAFETY: forwarded contract.
    unsafe { text(name) }.is_some_and(|name| HostName::parse(name).is_ok())
}

/// Start a session; on `FR_OK` writes its handle to `out`.
///
/// # Safety
/// `config` and `out` are NULL or valid; the strings `config` points to are
/// NULL or NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_start(config: *const FrConfig, out: *mut FrHandle) -> i32 {
    // SAFETY: NULL or valid per contract.
    let (Some(config), false) = (unsafe { config.as_ref() }, out.is_null()) else {
        return FR_INVALID;
    };
    // SAFETY: forwarded contract.
    let Some((host, pin)) = (unsafe { destination(config) }) else {
        return FR_INVALID;
    };
    // Refuse early when every slot is taken (registering the started
    // session below still refuses if another start won the last slot).
    if lookup_free_slots() == 0 {
        return FR_BUSY;
    }
    let Ok(started) = Session::start(Config {
        host,
        port: config.port,
        pin,
        control: config.control,
        direct: config.direct,
    }) else {
        return FR_BUSY;
    };
    let Some(handle) = register(started) else {
        // Lost a race for the last slot: the dropped session closes itself.
        return FR_BUSY;
    };
    // SAFETY: `out` is non-null and writable per contract.
    unsafe { out.write(handle) };
    FR_OK
}

/// The checked name, pin and port of `config`: a valid name, a pin (when
/// given) for exactly that name, required when `direct`, and a nonzero port.
///
/// # Safety
/// The strings `config` points to are NULL or NUL-terminated.
unsafe fn destination(config: &FrConfig) -> Option<(HostName, Option<HostPin>)> {
    // SAFETY: forwarded contract.
    let host = HostName::parse(unsafe { text(config.host) }?).ok()?;
    let pin = if config.pin.is_null() {
        None
    } else {
        // SAFETY: forwarded contract.
        let pin = HostPin::from_str(unsafe { text(config.pin) }?).ok()?;
        if pin.name() != &host {
            return None;
        }
        Some(pin)
    };
    (config.port != 0 && (pin.is_some() || !config.direct)).then_some((host, pin))
}

/// Most directory fetches in flight at once.
pub const MAX_FETCHES: usize = 4;
static FETCHES: AtomicUsize = AtomicUsize::new(0);

/// Called once with a fetch's JSON result (valid only during the call).
pub type FrDirectoryDone = Option<unsafe extern "C" fn(context: *mut c_void, json: *const c_char)>;

/// The app's callback and its context, moved to the fetch thread.
struct Done {
    context: *mut c_void,
    call: unsafe extern "C" fn(*mut c_void, *const c_char),
}
// SAFETY: `fr_directory_fetch`'s contract: `context` may be used from any
// thread until `call` returns, and `call` is safe to invoke from any thread.
unsafe impl Send for Done {}

/// Releases a fetch slot when its thread ends (or never started).
struct Fetching;
impl Drop for Fetching {
    fn drop(&mut self) {
        FETCHES.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Ask `config`'s host for its tailnet directory (`frd run --directory`):
/// the host itself and this user's own untagged machines. `config.control`
/// is ignored. On `FR_OK`, `done` is called exactly once, on another
/// thread, with a NUL-terminated JSON document (docs/ios-client.md) valid
/// only during the call; on any other status it is never called.
///
/// # Safety
/// `config` is NULL or valid, its strings NULL or NUL-terminated. `context`
/// may be used from any thread until `done` returns, and `done` (when not
/// NULL) is safe to call from any thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_directory_fetch(
    config: *const FrConfig,
    context: *mut c_void,
    done: FrDirectoryDone,
) -> i32 {
    // SAFETY: NULL or valid per contract.
    let (Some(config), Some(call)) = (unsafe { config.as_ref() }, done) else {
        return FR_INVALID;
    };
    // SAFETY: forwarded contract.
    let Some((host, pin)) = (unsafe { destination(config) }) else {
        return FR_INVALID;
    };
    let request = directory::Request {
        host,
        port: config.port,
        pin,
        direct: config.direct,
    };
    if FETCHES
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < MAX_FETCHES).then_some(n + 1)
        })
        .is_err()
    {
        return FR_BUSY;
    }
    let slot = Fetching;
    let done = Done { context, call };
    let spawned = std::thread::Builder::new()
        .name("fr-directory".into())
        .stack_size(4 * 1024 * 1024)
        .spawn(move || {
            let _slot = slot;
            let done = done;
            // JSON never holds a NUL (serde escapes it); fall back to an
            // internal failure rather than skip the one callback. A panic in
            // the fetch (a debug build unwinds) still calls back once.
            let fetched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                directory::fetch(request)
            }));
            let json = fetched
                .ok()
                .and_then(|json| CString::new(json).ok())
                .unwrap_or_else(|| {
                    CString::from(c"{\"status\":\"failed\",\"end\":10,\"detail\":\"Internal\"}")
                });
            // SAFETY: per this function's contract; the string outlives the call.
            unsafe { (done.call)(done.context, json.as_ptr()) };
        });
    if spawned.is_err() {
        return FR_BUSY;
    }
    FR_OK
}

fn lookup_free_slots() -> usize {
    let slots = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    MAX_SESSIONS - slots.iter().filter(|slot| slot.session.is_some()).count()
}

/// Take the oldest pending event. Returns false when none is pending or the
/// handle is stale.
///
/// # Safety
/// `event` is NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_poll_event(
    handle: FrHandle,
    event: *mut crate::events::Event,
) -> bool {
    let Some(s) = lookup(handle) else {
        return false;
    };
    if event.is_null() {
        return false;
    }
    let Some(next) = s.shared().events.poll() else {
        return false;
    };
    // SAFETY: non-null and writable per contract.
    unsafe { event.write(next) };
    true
}

/// Copy the host pin observed by this session (for the app to store with
/// the saved host). Returns its length, 0 when none is known yet.
///
/// # Safety
/// `buffer` is NULL or writable for `capacity` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_copy_pin(
    handle: FrHandle,
    buffer: *mut c_char,
    capacity: usize,
) -> usize {
    let pin = lookup(handle)
        .and_then(|s| s.shared().pin())
        .unwrap_or_default();
    // SAFETY: forwarded contract.
    unsafe { copy_out(pin.as_bytes(), buffer, capacity) }
}

/// Copy the ended session's typed failure (content-free; for diagnostics
/// the user chooses to share). Returns its length, 0 when none.
///
/// # Safety
/// As `fr_session_copy_pin`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_copy_detail(
    handle: FrHandle,
    buffer: *mut c_char,
    capacity: usize,
) -> usize {
    let detail = lookup(handle)
        .and_then(|s| s.shared().detail())
        .unwrap_or_default();
    // SAFETY: forwarded contract.
    unsafe { copy_out(detail.as_bytes(), buffer, capacity) }
}

/// The newest decoded picture when it is newer than `after_sequence`, as a
/// +1 `CVPixelBufferRef` (IOSurface-backed NV12), else NULL. `info`
/// receives its sequence, frame id and size.
///
/// # Safety
/// `info` is NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_copy_frame(
    handle: FrHandle,
    after_sequence: u64,
    info: *mut FrFrameInfo,
) -> *mut c_void {
    let Some(s) = lookup(handle) else {
        return core::ptr::null_mut();
    };
    let shared = s.shared();
    let Some((image, frame)) = shared
        .frames
        .take_newer(after_sequence, shared.full_range())
    else {
        return core::ptr::null_mut();
    };
    if !info.is_null() {
        // SAFETY: non-null and writable per contract.
        unsafe { info.write(frame.into()) };
    }
    image
}

/// Report that the picture with frame id `frame` was drawn into a drawable
/// that was presented. This is the session's evidence that the user can see
/// the host; control is requested and kept only on it.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_frame_drawn(handle: FrHandle, frame: u64) {
    if let Some(s) = lookup(handle) {
        s.shared().frames.mark_drawn(frame);
    }
}

/// The session view is (or is no longer) on screen in an active app.
/// Becoming invisible ends control at once.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_set_visible(handle: FrHandle, visible: bool) {
    if let Some(s) = lookup(handle) {
        s.shared().set_visible(visible);
    }
}

/// Ask the host for control (sent once a drawn picture is on screen). Only
/// a session started with `control` can be granted.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_request_control(handle: FrHandle) -> i32 {
    let Some(s) = lookup(handle) else {
        return FR_STALE;
    };
    if s.shared().finished() {
        return FR_CLOSED;
    }
    if !s.control() {
        return FR_UNSUPPORTED;
    }
    s.shared().request_control();
    FR_OK
}

/// Give control back. Ending control ends the session (frd's contract): this
/// starts the orderly close; reconnect to view again.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_release_control(handle: FrHandle) {
    if let Some(s) = lookup(handle) {
        s.shared().release_control();
    }
}

fn input(handle: FrHandle, act: impl FnOnce(&Session) -> InputResult) -> i32 {
    lookup(handle).map_or(FR_STALE, |s| code(act(&s)))
}

/// Move the host pointer to encoded picture pixel `(x, y)`.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_pointer(handle: FrHandle, x: f64, y: f64) -> i32 {
    input(handle, |s| s.shared().pointer(x, y))
}

/// Press or release pointer button `button` (0 primary, 1 secondary,
/// 2 middle, 3 back, 4 forward) at encoded picture pixel `(x, y)`.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_button(
    handle: FrHandle,
    button: u32,
    pressed: bool,
    x: f64,
    y: f64,
) -> i32 {
    input(handle, |s| s.shared().button(button, pressed, x, y))
}

/// Scroll by `dx`, `dy` lines (fractional; positive y scrolls the content
/// toward the end) at encoded picture pixel `(x, y)`. Whole lines are sent;
/// `FR_PENDING` means the amount was kept for the next call.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_scroll(handle: FrHandle, dx: f64, dy: f64, x: f64, y: f64) -> i32 {
    input(handle, |s| s.shared().scroll(dx, dy, x, y))
}

/// A scroll gesture ended: drop its fractional remainder.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_scroll_end(handle: FrHandle) {
    if let Some(s) = lookup(handle) {
        s.shared().scroll_end();
    }
}

/// Press or release the key with USB HID keyboard usage `usage` (page 7;
/// modifiers 0xE0..=0xE7). The host never sees client-side repeat.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_key(handle: FrHandle, usage: u16, pressed: bool) -> i32 {
    input(handle, |s| s.shared().key(usage, pressed))
}

/// Commit Unicode text. `FR_UNSUPPORTED` unless the host granted text
/// input; the app never guesses a keyboard layout to type it instead.
///
/// # Safety
/// `utf8` is NULL or NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_text(handle: FrHandle, utf8: *const c_char) -> i32 {
    // SAFETY: forwarded contract.
    let Some(value) = (unsafe { text(utf8) }) else {
        return FR_INVALID;
    };
    input(handle, |s| s.shared().text(value))
}

/// Release every key this session holds down.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_release_all(handle: FrHandle) -> i32 {
    input(handle, |s| s.shared().release_all())
}

/// The remote cursor's state (for hosts that send it apart from the
/// picture). Returns false before any cursor is known.
///
/// # Safety
/// `out` is NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_cursor(handle: FrHandle, out: *mut FrCursor) -> bool {
    let Some(s) = lookup(handle) else {
        return false;
    };
    let Ok(cursor) = s.shared().cursor.lock() else {
        return false;
    };
    if out.is_null() || cursor.generation == 0 {
        return false;
    }
    let value = FrCursor {
        generation: cursor.generation,
        x: cursor.x,
        y: cursor.y,
        width: cursor.width,
        height: cursor.height,
        hotspot_x: cursor.hotspot_x,
        hotspot_y: cursor.hotspot_y,
        visible: u32::from(cursor.visible),
        reserved: 0,
    };
    // SAFETY: non-null and writable per contract.
    unsafe { out.write(value) };
    true
}

/// Copy the cursor shape (RGBA, row-major, `width * height * 4` bytes).
/// Returns the shape's length; copies only when `capacity` holds all of it.
///
/// # Safety
/// `buffer` is NULL or writable for `capacity` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_copy_cursor_rgba(
    handle: FrHandle,
    buffer: *mut u8,
    capacity: usize,
) -> usize {
    let Some(s) = lookup(handle) else {
        return 0;
    };
    let Ok(cursor) = s.shared().cursor.lock() else {
        return 0;
    };
    let rgba = &cursor.rgba;
    if !buffer.is_null() && capacity >= rgba.len() {
        // SAFETY: writable for `capacity` >= len bytes; Rust memory does not
        // overlap the caller's buffer.
        unsafe { core::ptr::copy_nonoverlapping(rgba.as_ptr(), buffer, rgba.len()) };
    }
    rgba.len()
}

/// Content-free session statistics. Returns false for a stale handle.
///
/// # Safety
/// `out` is NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_stats(handle: FrHandle, out: *mut FrStats) -> bool {
    let Some(s) = lookup(handle) else {
        return false;
    };
    if out.is_null() {
        return false;
    }
    let shared = s.shared();
    let (decoded, decode_micros) = shared.frames.decode_stats();
    let stats = FrStats {
        presented: shared.frames.presented(),
        drawn_frame: shared.frames.drawn_raw(),
        decoded,
        decode_micros,
        first_view_micros: shared.first_view_micros(),
        hardware: shared.hardware(),
        controlled: u32::from(shared.controlled()),
        end: shared.end_code(),
        reserved: 0,
    };
    // SAFETY: non-null and writable per contract.
    unsafe { out.write(stats) };
    true
}

/// Start the orderly close (idempotent; never blocks). The session reports
/// `FR_EVENT_ENDED` when it finished.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_close(handle: FrHandle) -> i32 {
    lookup(handle).map_or(FR_STALE, |s| {
        s.shared().close();
        FR_OK
    })
}

/// Close (if still open) and release the handle; later uses of it, and a
/// second free, return `FR_STALE`. The session's thread finishes its bounded
/// close on its own; this never blocks.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_free(handle: FrHandle) -> i32 {
    // Dropped outside the registry lock: Session's drop only signals.
    let removed = unregister(handle);
    if removed.is_some() { FR_OK } else { FR_STALE }
}

/// Keeps the exported status constants and the header in step (tests).
pub const STATUS: [(&str, i32); 10] = [
    ("FR_OK", FR_OK),
    ("FR_PENDING", FR_PENDING),
    ("FR_INVALID", FR_INVALID),
    ("FR_NOT_CONTROLLED", FR_NOT_CONTROLLED),
    ("FR_OUTSIDE", FR_OUTSIDE),
    ("FR_UNSUPPORTED", FR_UNSUPPORTED),
    ("FR_CONTROL_LOST", FR_CONTROL_LOST),
    ("FR_CLOSED", FR_CLOSED),
    ("FR_BUSY", FR_BUSY),
    ("FR_STALE", FR_STALE),
];

#[cfg(test)]
mod tests;
