//! Optional iOS clipboard setup and content-free original-owner status. No
//! clipboard text crosses the app ABI or enters an event/diagnostic document.
use super::{
    FR_BUSY, FR_CLOSED, FR_DISPLAY_CHOOSE, FR_DISPLAY_PRIMARY, FR_INVALID, FR_OK, FR_STALE,
    FR_UNSUPPORTED, FrConfig, FrHandle, download, lookup, start,
};
use crate::session::DisplayChoice;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct FrClipboardStatus {
    pub requested: u32,
    pub enabled: u32,
    pub started: u32,
    /// 0 no refusal, 1 unavailable/declined, 2 local Off/privacy lapse,
    /// 3 native/session failure. No platform strings or clipboard bytes.
    pub reason: u32,
    pub received_submitted: u64,
    pub received_unknown: u64,
    pub received_refused: u64,
}

/// Start with explicit text-clipboard sharing and optional file capabilities.
/// `destination == 0` chooses no receive directory. A nonzero destination is
/// consumed on EVERY call; it can never be reused after a refused start.
/// Existing start entry points retain their original clipboard-free behavior.
///
/// # Safety
/// As `fr_session_start_with_display`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_start_with_clipboard(
    config: *const FrConfig,
    mode: u32,
    upload: bool,
    destination: u64,
    out: *mut FrHandle,
) -> i32 {
    let receive = if destination == 0 {
        None
    } else {
        match download::take_destination(destination) {
            Ok(owner) => Some(owner),
            Err(error) => return error,
        }
    };
    if !cfg!(target_os = "ios") {
        return FR_UNSUPPORTED;
    }
    let display = match mode {
        FR_DISPLAY_PRIMARY => DisplayChoice::Primary,
        FR_DISPLAY_CHOOSE => DisplayChoice::ChooseIfMultiple,
        _ => return FR_INVALID,
    };
    // SAFETY: forwarded configuration/string/output contract.
    unsafe { start(config, display, upload, receive, true, out) }
}

/// # Safety
/// `out` is NULL or writable for one `FrClipboardStatus`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_clipboard_status(
    handle: FrHandle,
    out: *mut FrClipboardStatus,
) -> bool {
    if out.is_null() {
        return false;
    }
    let Some(session) = lookup(handle) else {
        return false;
    };
    let shared = session.shared();
    let report = shared.clipboard_report();
    let enabled = shared.clipboard_enabled();
    let reason = match report.reason {
        Some(
            frd::clipboard_quic::Error::NotNegotiated | frd::clipboard_quic::Error::ConsentRequired,
        ) => 1,
        Some(frd::clipboard_quic::Error::SwitchChanged) => 2,
        Some(_) => 3,
        None if report.requested && !enabled => 2,
        None => 0,
    };
    // SAFETY: non-null writable output supplied by the caller.
    unsafe {
        out.write(FrClipboardStatus {
            requested: u32::from(report.requested),
            enabled: u32::from(enabled),
            started: u32::from(report.native_started),
            reason,
            received_submitted: report.received_submitted,
            received_unknown: report.received_unknown,
            received_refused: report.received_refused,
        });
    }
    true
}

/// Report only actual presence of the persistent clipboard control in the
/// active app. Silence over 300 ms or false is terminal for this owner.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_clipboard_presence(handle: FrHandle, visible: bool) -> i32 {
    let Some(session) = lookup(handle) else {
        return FR_STALE;
    };
    let shared = session.shared();
    if shared.finished() {
        return FR_CLOSED;
    }
    if !shared.clipboard_report().requested {
        return FR_UNSUPPORTED;
    }
    shared.clipboard_presence(visible);
    if shared.clipboard_enabled() {
        FR_OK
    } else {
        FR_CLOSED
    }
}

/// Turn sharing off for this original connection. No re-enable or automatic
/// clipboard restart is possible; ordinary viewing/control remain available.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_stop_clipboard(handle: FrHandle) -> i32 {
    let Some(session) = lookup(handle) else {
        return FR_STALE;
    };
    let shared = session.shared();
    if !shared.clipboard_report().requested {
        return FR_UNSUPPORTED;
    }
    shared.stop_clipboard();
    if shared.clipboard_enabled() {
        FR_BUSY
    } else {
        FR_OK
    }
}
