//! Explicit file selection and bounded progress for the original C session.
//! All file preparation runs on the caller's bounded worker, never its UI.
use super::{
    FR_BUSY, FR_CLOSED, FR_DISPLAY_CHOOSE, FR_DISPLAY_PRIMARY, FR_INVALID, FR_OK, FR_STALE,
    FR_UNSUPPORTED, FrConfig, FrHandle, code, copy_out, lookup, start, text,
};
use crate::{
    session::{DisplayChoice, UploadError},
    upload_selection::Error as ChoiceError,
};
use core::ffi::c_char;
use frd::session_startup::FileSendError;
use std::path::Path;

/// Includes room for nine original receipts and their bounded typed reasons.
pub const FR_UPLOAD_STATUS_MAX_BYTES: usize = 16_384;

fn upload_code(error: UploadError) -> i32 {
    match error {
        UploadError::Unavailable => FR_UNSUPPORTED,
        UploadError::Input(result) => code(result),
        UploadError::Choice(
            ChoiceError::Busy | ChoiceError::PreparationStarted | ChoiceError::NotPrepared,
        )
        | UploadError::Transfer(FileSendError::Busy) => FR_BUSY,
        UploadError::Closed
        | UploadError::Choice(ChoiceError::Exhausted)
        | UploadError::Transfer(FileSendError::Closed | FileSendError::Cancelled) => FR_CLOSED,
        UploadError::Choice(_) | UploadError::Transfer(FileSendError::Expired) => FR_STALE,
        UploadError::Source | UploadError::Transfer(_) => FR_INVALID,
    }
}

/// Start a control-capable session with optional, empty file-upload negotiation.
/// Selects no file, requests no control, and never replays a file on reconnect.
/// The existing start functions retain their file-free behavior and `FrConfig` ABI.
///
/// # Safety
/// As `fr_session_start_with_display`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_start_with_upload(
    config: *const FrConfig,
    mode: u32,
    out: *mut FrHandle,
) -> i32 {
    let display = match mode {
        FR_DISPLAY_PRIMARY => DisplayChoice::Primary,
        FR_DISPLAY_CHOOSE => DisplayChoice::ChooseIfMultiple,
        _ => return FR_INVALID,
    };
    // SAFETY: forwarded configuration/string/output contract.
    unsafe { start(config, display, true, None, false, out) }
}

/// Reserve one choice before opening the native picker while observing.
/// On success writes a non-reused token, bound to this attempt for 120 seconds.
///
/// # Safety
/// `out` is NULL or writable for one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_begin_upload_selection(handle: FrHandle, out: *mut u64) -> i32 {
    if out.is_null() {
        return FR_INVALID;
    }
    let Some(session) = lookup(handle) else {
        return FR_STALE;
    };
    match session.shared().begin_upload_selection() {
        Ok(token) => {
            // SAFETY: non-null and writable per caller contract.
            unsafe { out.write(token) };
            FR_OK
        }
        Err(error) => upload_code(error),
    }
}

/// Pin one regular local file; call only from a bounded background worker.
/// The caller retains its security-scoped URL permission during this call and,
/// on a later successful send, until `upload_sources_released` is true.
/// No file contents enter the network before a separate explicit Send and grant.
///
/// # Safety
/// `path` is NULL or a NUL-terminated UTF-8 string valid for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_prepare_upload(
    handle: FrHandle,
    token: u64,
    path: *const c_char,
) -> i32 {
    let Some(session) = lookup(handle) else {
        return FR_STALE;
    };
    // SAFETY: forwarded string contract.
    let Some(path) = (unsafe { text(path) }).filter(|path| !path.is_empty() && path.len() <= 4096)
    else {
        return FR_INVALID;
    };
    session
        .shared()
        .prepare_upload(token, Path::new(path))
        .map_or_else(upload_code, |()| FR_OK)
}

/// Retire only the original local choice. A preparation still in flight must
/// return before the shell releases its URL permission or starts another one.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_discard_upload_selection(handle: FrHandle, token: u64) -> i32 {
    let Some(session) = lookup(handle) else {
        return FR_STALE;
    };
    if session.shared().discard_upload_selection(token) {
        FR_OK
    } else {
        FR_STALE
    }
}

/// A fresh explicit Send on the actual controlled view. Consumes the selection
/// on every attempted send; callers never retry `FR_BUSY` or any other refusal.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_send_upload(handle: FrHandle, token: u64) -> i32 {
    let Some(session) = lookup(handle) else {
        return FR_STALE;
    };
    session
        .shared()
        .send_upload(token)
        .map_or_else(upload_code, |()| FR_OK)
}

/// Cancel the exact batch displayed by the UI, leaving the desktop running.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_cancel_upload(handle: FrHandle, batch: u32) -> i32 {
    upload_batch(handle, batch, |session, batch| {
        session.cancel_upload_batch(batch)
    })
}

/// Pause or resume the exact original batch; no deadline or authority renews.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_pause_upload(handle: FrHandle, batch: u32, paused: bool) -> i32 {
    upload_batch(handle, batch, |session, batch| {
        session.pause_upload_batch(batch, paused)
    })
}

fn upload_batch(
    handle: FrHandle,
    batch: u32,
    change: impl FnOnce(&crate::session::Shared, u8) -> bool,
) -> i32 {
    let Ok(batch) = u8::try_from(batch) else {
        return FR_INVALID;
    };
    if batch == 0 || usize::from(batch) > crate::session::MAX_UPLOADS {
        return FR_INVALID;
    }
    let Some(session) = lookup(handle) else {
        return FR_STALE;
    };
    if change(session.shared(), batch) {
        FR_OK
    } else {
        FR_BUSY
    }
}

/// Positive original disk-source collection, including after failed bounded
/// cleanup. False for a stale handle. Keep the handle and native URL scopes
/// until true; `fr_session_free` itself never joins a worker.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_upload_sources_released(handle: FrHandle) -> bool {
    lookup(handle).is_some_and(|session| session.shared().upload_sources_released())
}

/// Copy version-1 content-free JSON, with the existing string-copy convention:
/// returns required bytes excluding NUL, always terminates a nonempty buffer.
/// Zero means a stale handle or unavailable document, not successful transfer.
/// Queued bytes, host publication, durability and cleanup remain distinct.
///
/// # Safety
/// `buffer` is NULL or writable for `capacity` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_copy_upload_status(
    handle: FrHandle,
    buffer: *mut c_char,
    capacity: usize,
) -> usize {
    let document = lookup(handle)
        .map(|session| session.shared().upload_document().to_string())
        .filter(|document| document.len() < FR_UPLOAD_STATUS_MAX_BYTES)
        .unwrap_or_default();
    // SAFETY: forwarded bounded output contract.
    unsafe { copy_out(document.as_bytes(), buffer, capacity) }
}
