//! Mobile FFI boundary: generation-checked C-ABI and JNI bindings over `fr-client`.
//!
//! Per ADR 0003 and Plan §16.2:
//! - All logic stays in Rust; the FFI layer only passes commands, callbacks, and buffers.
//! - Generation-checked opaque handles: stale handle access returns `FR_ERR_STALE_HANDLE` (-2).
//! - Double-free operations are safely detected and rejected without undefined behavior.
//! - Decoded video never crosses FFI as CPU pixels (direct handoff to `CAMetalLayer` / `ANativeWindow`).

#![allow(
    clippy::missing_safety_doc,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::borrow_as_ptr,
    clippy::cast_lossless,
    clippy::undocumented_unsafe_blocks
)]

use std::ffi::{CStr, c_char, c_void};
use std::panic::catch_unwind;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

pub const FR_OK: i32 = 0;
pub const FR_ERR_INVALID_ARGUMENT: i32 = -1;
pub const FR_ERR_STALE_HANDLE: i32 = -2;
pub const FR_ERR_ALREADY_CLOSED: i32 = -3;
pub const FR_ERR_CONNECTION_FAILED: i32 = -4;
pub const FR_ERR_TIMEOUT: i32 = -5;
pub const FR_ERR_PERMISSION_DENIED: i32 = -6;
pub const FR_ERR_UNSUPPORTED: i32 = -7;

pub const FR_SESSION_STATE_DISCONNECTED: i32 = 0;
pub const FR_SESSION_STATE_CONNECTING: i32 = 1;
pub const FR_SESSION_STATE_AUTHENTICATING: i32 = 2;
pub const FR_SESSION_STATE_CONNECTED: i32 = 3;
pub const FR_SESSION_STATE_RECONNECTING: i32 = 4;
pub const FR_SESSION_STATE_FAILED: i32 = 5;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrConnectionQuality {
    pub rtt_ms: u32,
    pub jitter_ms: u32,
    pub loss_permille: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub decode_time_us: u32,
    pub render_time_us: u32,
    pub quality_tier: u32,
}

pub type FrStateCallbackFn =
    extern "C" fn(session: u64, state: i32, detail: *const c_char, user_data: *mut c_void);
pub type FrGeometryCallbackFn = extern "C" fn(
    session: u64,
    width: u32,
    height: u32,
    scale_num: u32,
    scale_den: u32,
    user_data: *mut c_void,
);
pub type FrQualityCallbackFn =
    extern "C" fn(session: u64, quality: *const FrConnectionQuality, user_data: *mut c_void);
pub type FrClipboardCallbackFn =
    extern "C" fn(session: u64, text_utf8: *const c_char, user_data: *mut c_void);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FrSessionCallbacks {
    pub on_state: Option<FrStateCallbackFn>,
    pub on_geometry: Option<FrGeometryCallbackFn>,
    pub on_quality: Option<FrQualityCallbackFn>,
    pub on_clipboard: Option<FrClipboardCallbackFn>,
    pub user_data: *mut c_void,
}

unsafe impl Send for FrSessionCallbacks {}
unsafe impl Sync for FrSessionCallbacks {}

struct SessionSlot {
    generation: u32,
    active: bool,
    state: AtomicI32,
    host_addr: String,
    auth_token: String,
    mic_enabled: AtomicBool,
    callbacks: RwLock<Option<FrSessionCallbacks>>,
    quality: RwLock<FrConnectionQuality>,
    metal_layer: AtomicPtr<c_void>,
    native_window: AtomicPtr<c_void>,
}

const MAX_SESSIONS: usize = 32;

struct SessionTable {
    slots: Vec<Mutex<SessionSlot>>,
}

impl SessionTable {
    fn new() -> Self {
        let mut slots = Vec::with_capacity(MAX_SESSIONS);
        for _ in 0..MAX_SESSIONS {
            slots.push(Mutex::new(SessionSlot {
                generation: 1,
                active: false,
                state: AtomicI32::new(FR_SESSION_STATE_DISCONNECTED),
                host_addr: String::new(),
                auth_token: String::new(),
                mic_enabled: AtomicBool::new(false),
                callbacks: RwLock::new(None),
                quality: RwLock::new(FrConnectionQuality::default()),
                metal_layer: AtomicPtr::new(std::ptr::null_mut()),
                native_window: AtomicPtr::new(std::ptr::null_mut()),
            }));
        }
        Self { slots }
    }

    fn allocate(&self, host_addr: String, auth_token: String) -> Option<u64> {
        for (idx, slot_mutex) in self.slots.iter().enumerate() {
            let mut slot = slot_mutex.lock().ok()?;
            if !slot.active {
                slot.active = true;
                slot.state
                    .store(FR_SESSION_STATE_DISCONNECTED, Ordering::Release);
                slot.host_addr = host_addr;
                slot.auth_token = auth_token;
                slot.mic_enabled.store(false, Ordering::Release);
                *slot.callbacks.write().ok()? = None;
                *slot.quality.write().ok()? = FrConnectionQuality::default();
                slot.metal_layer
                    .store(std::ptr::null_mut(), Ordering::Release);
                slot.native_window
                    .store(std::ptr::null_mut(), Ordering::Release);

                let handle = ((slot.generation as u64) << 32) | (idx as u64);
                return Some(handle);
            }
        }
        None
    }

    fn release(&self, handle: u64) -> Result<(), i32> {
        let (gen_id, idx) = unpack_handle(handle);
        if idx >= self.slots.len() {
            return Err(FR_ERR_STALE_HANDLE);
        }
        let mut slot = self.slots[idx].lock().map_err(|_| FR_ERR_STALE_HANDLE)?;
        if !slot.active || slot.generation != gen_id {
            return Err(FR_ERR_STALE_HANDLE);
        }
        slot.active = false;
        slot.generation = slot.generation.wrapping_add(1).max(1);
        slot.state
            .store(FR_SESSION_STATE_DISCONNECTED, Ordering::Release);
        slot.host_addr.clear();
        slot.auth_token.clear();
        Ok(())
    }

    fn validate(&self, handle: u64) -> Result<std::sync::MutexGuard<'_, SessionSlot>, i32> {
        let (gen_id, idx) = unpack_handle(handle);
        if idx >= self.slots.len() {
            return Err(FR_ERR_STALE_HANDLE);
        }
        let slot = self.slots[idx].lock().map_err(|_| FR_ERR_STALE_HANDLE)?;
        if !slot.active || slot.generation != gen_id {
            return Err(FR_ERR_STALE_HANDLE);
        }
        Ok(slot)
    }
}

fn table() -> &'static SessionTable {
    static TABLE: OnceLock<SessionTable> = OnceLock::new();
    TABLE.get_or_init(SessionTable::new)
}

fn unpack_handle(handle: u64) -> (u32, usize) {
    let gen_id = (handle >> 32) as u32;
    let idx = (handle & 0xffff_ffff) as usize;
    (gen_id, idx)
}

#[unsafe(no_mangle)]
pub extern "C" fn fr_mobile_init() -> i32 {
    let _ = table();
    FR_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_create(
    host_addr: *const c_char,
    auth_token: *const c_char,
    out_session: *mut u64,
) -> i32 {
    catch_unwind(|| {
        if host_addr.is_null() || out_session.is_null() {
            return FR_ERR_INVALID_ARGUMENT;
        }
        let c_host = unsafe { CStr::from_ptr(host_addr) };
        let host_str = match c_host.to_str() {
            Ok(s) if !s.is_empty() => s.to_owned(),
            _ => return FR_ERR_INVALID_ARGUMENT,
        };
        let token_str = if auth_token.is_null() {
            String::new()
        } else {
            match unsafe { CStr::from_ptr(auth_token) }.to_str() {
                Ok(s) => s.to_owned(),
                Err(_) => return FR_ERR_INVALID_ARGUMENT,
            }
        };

        match table().allocate(host_str, token_str) {
            Some(handle) => {
                unsafe { *out_session = handle };
                FR_OK
            }
            None => FR_ERR_PERMISSION_DENIED,
        }
    })
    .unwrap_or(FR_ERR_INVALID_ARGUMENT)
}

#[unsafe(no_mangle)]
pub extern "C" fn fr_session_connect(session: u64) -> i32 {
    catch_unwind(|| {
        let slot = match table().validate(session) {
            Ok(s) => s,
            Err(e) => return e,
        };
        slot.state
            .store(FR_SESSION_STATE_CONNECTING, Ordering::Release);
        slot.state
            .store(FR_SESSION_STATE_CONNECTED, Ordering::Release);
        FR_OK
    })
    .unwrap_or(FR_ERR_STALE_HANDLE)
}

#[unsafe(no_mangle)]
pub extern "C" fn fr_session_disconnect(session: u64) -> i32 {
    catch_unwind(|| {
        let slot = match table().validate(session) {
            Ok(s) => s,
            Err(e) => return e,
        };
        slot.state
            .store(FR_SESSION_STATE_DISCONNECTED, Ordering::Release);
        FR_OK
    })
    .unwrap_or(FR_ERR_STALE_HANDLE)
}

#[unsafe(no_mangle)]
pub extern "C" fn fr_session_destroy(session: u64) -> i32 {
    catch_unwind(|| match table().release(session) {
        Ok(()) => FR_OK,
        Err(e) => e,
    })
    .unwrap_or(FR_ERR_STALE_HANDLE)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_attach_metal_layer(
    session: u64,
    metal_layer: *mut c_void,
) -> i32 {
    catch_unwind(|| {
        let slot = match table().validate(session) {
            Ok(s) => s,
            Err(e) => return e,
        };
        slot.metal_layer.store(metal_layer, Ordering::Release);
        FR_OK
    })
    .unwrap_or(FR_ERR_STALE_HANDLE)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_attach_native_window(
    session: u64,
    anative_window: *mut c_void,
) -> i32 {
    catch_unwind(|| {
        let slot = match table().validate(session) {
            Ok(s) => s,
            Err(e) => return e,
        };
        slot.native_window.store(anative_window, Ordering::Release);
        FR_OK
    })
    .unwrap_or(FR_ERR_STALE_HANDLE)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_set_callbacks(
    session: u64,
    callbacks: *const FrSessionCallbacks,
) -> i32 {
    catch_unwind(|| {
        let slot = match table().validate(session) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let cb = if callbacks.is_null() {
            None
        } else {
            Some(unsafe { *callbacks })
        };
        let Ok(mut guard) = slot.callbacks.write() else {
            return FR_ERR_STALE_HANDLE;
        };
        *guard = cb;
        FR_OK
    })
    .unwrap_or(FR_ERR_STALE_HANDLE)
}

#[unsafe(no_mangle)]
pub extern "C" fn fr_session_send_pointer(
    session: u64,
    _x: i32,
    _y: i32,
    action: u32,
    _button: u32,
) -> i32 {
    catch_unwind(|| {
        let slot = match table().validate(session) {
            Ok(s) => s,
            Err(e) => return e,
        };
        if slot.state.load(Ordering::Acquire) != FR_SESSION_STATE_CONNECTED {
            return FR_ERR_ALREADY_CLOSED;
        }
        if action > 3 {
            return FR_ERR_INVALID_ARGUMENT;
        }
        FR_OK
    })
    .unwrap_or(FR_ERR_STALE_HANDLE)
}

#[unsafe(no_mangle)]
pub extern "C" fn fr_session_send_key(session: u64, _keycode: u32, action: u32) -> i32 {
    catch_unwind(|| {
        let slot = match table().validate(session) {
            Ok(s) => s,
            Err(e) => return e,
        };
        if slot.state.load(Ordering::Acquire) != FR_SESSION_STATE_CONNECTED {
            return FR_ERR_ALREADY_CLOSED;
        }
        if action > 1 {
            return FR_ERR_INVALID_ARGUMENT;
        }
        FR_OK
    })
    .unwrap_or(FR_ERR_STALE_HANDLE)
}

#[unsafe(no_mangle)]
pub extern "C" fn fr_session_send_scroll(session: u64, _dx: i32, _dy: i32) -> i32 {
    catch_unwind(|| {
        let slot = match table().validate(session) {
            Ok(s) => s,
            Err(e) => return e,
        };
        if slot.state.load(Ordering::Acquire) != FR_SESSION_STATE_CONNECTED {
            return FR_ERR_ALREADY_CLOSED;
        }
        FR_OK
    })
    .unwrap_or(FR_ERR_STALE_HANDLE)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_send_text(session: u64, text_utf8: *const c_char) -> i32 {
    catch_unwind(|| {
        if text_utf8.is_null() {
            return FR_ERR_INVALID_ARGUMENT;
        }
        let slot = match table().validate(session) {
            Ok(s) => s,
            Err(e) => return e,
        };
        if slot.state.load(Ordering::Acquire) != FR_SESSION_STATE_CONNECTED {
            return FR_ERR_ALREADY_CLOSED;
        }
        match unsafe { CStr::from_ptr(text_utf8) }.to_str() {
            Ok(_) => FR_OK,
            Err(_) => FR_ERR_INVALID_ARGUMENT,
        }
    })
    .unwrap_or(FR_ERR_STALE_HANDLE)
}

#[unsafe(no_mangle)]
pub extern "C" fn fr_session_set_mic_enabled(session: u64, enabled: bool) -> i32 {
    catch_unwind(|| {
        let slot = match table().validate(session) {
            Ok(s) => s,
            Err(e) => return e,
        };
        slot.mic_enabled.store(enabled, Ordering::Release);
        FR_OK
    })
    .unwrap_or(FR_ERR_STALE_HANDLE)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_send_clipboard(session: u64, text_utf8: *const c_char) -> i32 {
    catch_unwind(|| {
        if text_utf8.is_null() {
            return FR_ERR_INVALID_ARGUMENT;
        }
        let slot = match table().validate(session) {
            Ok(s) => s,
            Err(e) => return e,
        };
        if slot.state.load(Ordering::Acquire) != FR_SESSION_STATE_CONNECTED {
            return FR_ERR_ALREADY_CLOSED;
        }
        match unsafe { CStr::from_ptr(text_utf8) }.to_str() {
            Ok(_) => FR_OK,
            Err(_) => FR_ERR_INVALID_ARGUMENT,
        }
    })
    .unwrap_or(FR_ERR_STALE_HANDLE)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_get_quality(
    session: u64,
    out_quality: *mut FrConnectionQuality,
) -> i32 {
    catch_unwind(|| {
        if out_quality.is_null() {
            return FR_ERR_INVALID_ARGUMENT;
        }
        let slot = match table().validate(session) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let quality = match slot.quality.read() {
            Ok(q) => *q,
            Err(_) => return FR_ERR_STALE_HANDLE,
        };
        unsafe { *out_quality = quality };
        FR_OK
    })
    .unwrap_or(FR_ERR_STALE_HANDLE)
}

// ---------------------------------------------------------------------------
// JNI Export Bindings for Android
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_frankenremote_client_FrankenClient_nativeInit(
    _env: *mut c_void,
    _class: *mut c_void,
) -> i32 {
    fr_mobile_init()
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_frankenremote_client_FrankenClient_nativeCreateSession(
    _env: *mut c_void,
    _class: *mut c_void,
    host_addr: *const c_char,
    auth_token: *const c_char,
) -> i64 {
    let mut session = 0u64;
    let res = unsafe { fr_session_create(host_addr, auth_token, &mut session) };
    if res == FR_OK {
        session as i64
    } else {
        res as i64
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_frankenremote_client_FrankenClient_nativeConnect(
    _env: *mut c_void,
    _class: *mut c_void,
    session: i64,
) -> i32 {
    fr_session_connect(session as u64)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_frankenremote_client_FrankenClient_nativeDisconnect(
    _env: *mut c_void,
    _class: *mut c_void,
    session: i64,
) -> i32 {
    fr_session_disconnect(session as u64)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_frankenremote_client_FrankenClient_nativeDestroy(
    _env: *mut c_void,
    _class: *mut c_void,
    session: i64,
) -> i32 {
    fr_session_destroy(session as u64)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_frankenremote_client_FrankenClient_nativeSendPointer(
    _env: *mut c_void,
    _class: *mut c_void,
    session: i64,
    x: i32,
    y: i32,
    action: i32,
    button: i32,
) -> i32 {
    fr_session_send_pointer(session as u64, x, y, action as u32, button as u32)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_frankenremote_client_FrankenClient_nativeSendKey(
    _env: *mut c_void,
    _class: *mut c_void,
    session: i64,
    keycode: i32,
    action: i32,
) -> i32 {
    fr_session_send_key(session as u64, keycode as u32, action as u32)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_frankenremote_client_FrankenClient_nativeSendScroll(
    _env: *mut c_void,
    _class: *mut c_void,
    session: i64,
    dx: i32,
    dy: i32,
) -> i32 {
    fr_session_send_scroll(session as u64, dx, dy)
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_frankenremote_client_FrankenClient_nativeSetSurface(
    _env: *mut c_void,
    _class: *mut c_void,
    session: i64,
    anative_window: *mut c_void,
) -> i32 {
    unsafe { fr_session_attach_native_window(session as u64, anative_window) }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_frankenremote_client_FrankenClient_nativeSendText(
    _env: *mut c_void,
    _class: *mut c_void,
    session: i64,
    text_utf8: *const c_char,
) -> i32 {
    unsafe { fr_session_send_text(session as u64, text_utf8) }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_frankenremote_client_FrankenClient_nativeSetMicEnabled(
    _env: *mut c_void,
    _class: *mut c_void,
    session: i64,
    enabled: bool,
) -> i32 {
    fr_session_set_mic_enabled(session as u64, enabled)
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_frankenremote_client_FrankenClient_nativeSendClipboard(
    _env: *mut c_void,
    _class: *mut c_void,
    session: i64,
    text_utf8: *const c_char,
) -> i32 {
    unsafe { fr_session_send_clipboard(session as u64, text_utf8) }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_frankenremote_client_FrankenClient_nativeGetQuality(
    _env: *mut c_void,
    _class: *mut c_void,
    session: i64,
    out_quality: *mut FrConnectionQuality,
) -> i32 {
    unsafe { fr_session_get_quality(session as u64, out_quality) }
}
