//! Explicit local receive consent, prepared away from the UI and consumed by
//! one original C session. No destination can be reused on a later connection.
use super::{
    FR_BUSY, FR_DISPLAY_CHOOSE, FR_DISPLAY_PRIMARY, FR_INVALID, FR_OK, FR_STALE, FR_UNSUPPORTED,
    FrConfig, FrHandle, copy_out, lookup, start, text,
};
use crate::{
    download_selection::Destinations,
    session::{DisplayChoice, Download},
};
use core::ffi::c_char;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

pub const FR_DOWNLOAD_STATUS_MAX_BYTES: usize = 32_768;

static DESTINATIONS: Mutex<Destinations<Arc<Download>>> = Mutex::new(Destinations::new());

/// Reserve and pin one private destination. Call on a bounded background
/// worker: directory validation performs filesystem I/O, never network I/O.
/// Limits must be positive, and session bytes at least per-file bytes. Existing
/// names are kept with a conflict suffix; nothing is opened or executed.
///
/// # Safety
/// `path` is NULL or NUL-terminated UTF-8; `out` is NULL or writable for a `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_download_destination_prepare(
    path: *const c_char,
    max_file_bytes: u64,
    max_session_bytes: u64,
    out: *mut u64,
) -> i32 {
    if out.is_null() {
        return FR_INVALID;
    }
    // SAFETY: forwarded string contract.
    let Some(path) = (unsafe { text(path) }).filter(|path| !path.is_empty() && path.len() <= 4096)
    else {
        return FR_INVALID;
    };
    let token = match DESTINATIONS.lock() {
        Ok(mut destinations) => match destinations.reserve() {
            Some(token) => token,
            None => return FR_BUSY,
        },
        Err(_) => return FR_BUSY,
    };
    let selected = Download::select(
        Path::new(path),
        Some(max_file_bytes),
        Some(max_session_bytes),
        true,
    );
    let Ok(mut destinations) = DESTINATIONS.lock() else {
        return FR_BUSY;
    };
    let Ok(owner) = selected else {
        destinations.discard(token);
        return FR_INVALID;
    };
    if !destinations.prepared(token, owner) {
        return FR_STALE;
    }
    // SAFETY: non-null and writable per the caller's contract.
    unsafe { out.write(token) };
    FR_OK
}

/// Discard only this pending destination. Its one-use ID never becomes valid
/// again. Consumed tokens return stale without affecting their active session.
#[unsafe(no_mangle)]
pub extern "C" fn fr_download_destination_discard(token: u64) -> i32 {
    match DESTINATIONS.lock() {
        Ok(mut destinations) => {
            if destinations.discard(token) {
                FR_OK
            } else {
                FR_STALE
            }
        }
        Err(_) => FR_BUSY,
    }
}

/// Start one control-capable session with explicit file-receive consent and
/// optional empty upload negotiation. Consumes `destination` on EVERY attempt,
/// including invalid arguments and a failed start. There is no retry/replay of
/// this consent. No disk I/O occurs here. The old start APIs stay unchanged.
///
/// # Safety
/// As `fr_session_start_with_display`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_start_with_files(
    config: *const FrConfig,
    mode: u32,
    upload: bool,
    destination: u64,
    out: *mut FrHandle,
) -> i32 {
    let owner = match DESTINATIONS.lock() {
        Ok(mut destinations) => match destinations.take(destination) {
            Some(owner) => owner,
            None => return FR_STALE,
        },
        Err(_) => return FR_BUSY,
    };
    let display = match mode {
        FR_DISPLAY_PRIMARY => DisplayChoice::Primary,
        FR_DISPLAY_CHOOSE => DisplayChoice::ChooseIfMultiple,
        _ => return FR_INVALID,
    };
    // SAFETY: forwarded configuration/string/output contract.
    unsafe { start(config, display, upload, Some(owner), out) }
}

/// Stop only the original receive direction; already published files and
/// receipts remain. Cancellation never authorizes a replacement destination.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_cancel_download(handle: FrHandle) -> i32 {
    let Some(session) = lookup(handle) else {
        return FR_STALE;
    };
    let shared = session.shared();
    let Some(report) = shared.download_report() else {
        return FR_UNSUPPORTED;
    };
    if shared.cancel_download() || report.cancel_requested {
        FR_OK
    } else {
        FR_BUSY
    }
}

/// The original session is terminal and its receive disk cleanup is proven.
/// A terminal event alone, queued bytes or a failed cleanup is insufficient.
/// The shell retains its original handle and private destination until true.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_download_cleanup_complete(handle: FrHandle) -> bool {
    lookup(handle).is_some_and(|session| {
        let shared = session.shared();
        shared.finished()
            && shared
                .download_report()
                .is_some_and(|report| report.cleanup_complete())
    })
}

/// Copy bounded version-1 JSON: progress, typed absence, original publication
/// receipts and cleanup. No filenames, paths or contents. Required byte count
/// excludes NUL; zero denotes stale/unavailable, never successful receiving.
///
/// # Safety
/// `buffer` is NULL or writable for `capacity` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_copy_download_status(
    handle: FrHandle,
    buffer: *mut c_char,
    capacity: usize,
) -> usize {
    let document = lookup(handle)
        .and_then(|session| session.shared().download_report())
        .map(|report| report.document().to_string())
        .filter(|document| document.len() < FR_DOWNLOAD_STATUS_MAX_BYTES)
        .unwrap_or_default();
    // SAFETY: forwarded bounded output contract.
    unsafe { copy_out(document.as_bytes(), buffer, capacity) }
}
