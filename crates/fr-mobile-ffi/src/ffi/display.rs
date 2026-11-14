//! Approved display metadata crosses the C ABI with a non-reusable local
//! request token. Host aliases remain in the original Rust request, bounded
//! by the session's one pending catalog; a row is never rebound to a retry.
use super::{FR_CLOSED, FR_INVALID, FR_OK, FR_PENDING, FR_STALE, FrHandle, lookup};
use crate::session::{DisplayError, DisplayRequest, Session};
use fr_wire::display::Display;
use std::sync::Arc;

pub const FR_DISPLAY_PRIMARY: u32 = 0;
pub const FR_DISPLAY_CHOOSE: u32 = 1;
pub const FR_MAX_DISPLAYS: usize = fr_wire::display::MAX_DISPLAYS;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FrDisplayRequest {
    pub token: u64,
    pub count: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FrDisplayInfo {
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub logical_width: u32,
    pub logical_height: u32,
    pub x: i32,
    pub y: i32,
    pub scale_numerator: u32,
    pub scale_denominator: u32,
    pub rotation: u32,
    pub reserved: u32,
}

impl From<&Display> for FrDisplayInfo {
    fn from(display: &Display) -> Self {
        Self {
            pixel_width: display.pixel_width,
            pixel_height: display.pixel_height,
            logical_width: display.logical_width,
            logical_height: display.logical_height,
            x: display.x,
            y: display.y,
            scale_numerator: display.scale_numerator,
            scale_denominator: display.scale_denominator,
            rotation: u32::from(display.rotation),
            reserved: 0,
        }
    }
}

/// Copy the one pending approved catalog's descriptor, if any. Repeated
/// reads of the same request retain its token; no deadlines are extended.
///
/// # Safety
/// `out` is NULL or valid for one writable `FrDisplayRequest`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_display_request(
    handle: FrHandle,
    out: *mut FrDisplayRequest,
) -> bool {
    if out.is_null() {
        return false;
    }
    let Some(session) = lookup(handle) else {
        return false;
    };
    let Some(request) = session.shared().display_request() else {
        return false;
    };
    let Ok(count) = u32::try_from(request.displays().len()) else {
        return false;
    };
    // SAFETY: non-null and writable per the caller's contract.
    unsafe {
        out.write(FrDisplayRequest {
            token: request.token(),
            count,
            reserved: 0,
        });
    }
    true
}

fn current(handle: FrHandle, token: u64) -> Result<(Arc<Session>, DisplayRequest), i32> {
    let session = lookup(handle).ok_or(FR_STALE)?;
    let request = session
        .shared()
        .display_request()
        .filter(|request| request.token() == token)
        .ok_or(FR_STALE)?;
    Ok((session, request))
}

/// Copy exactly one original row; false leaves the output untouched. The
/// caller must discard a partial snapshot if any row cannot be copied.
///
/// # Safety
/// `out` is NULL or valid for one writable `FrDisplayInfo`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fr_session_display_info(
    handle: FrHandle,
    token: u64,
    index: u32,
    out: *mut FrDisplayInfo,
) -> bool {
    if out.is_null() {
        return false;
    }
    let Ok((_session, request)) = current(handle, token) else {
        return false;
    };
    let Some(display) = usize::try_from(index)
        .ok()
        .and_then(|index| request.displays().get(index))
    else {
        return false;
    };
    // SAFETY: non-null and writable per the caller's contract.
    unsafe { out.write(display.into()) };
    true
}

fn status(result: Result<(), DisplayError>) -> i32 {
    match result {
        Ok(()) => FR_OK,
        Err(DisplayError::Closed) => FR_CLOSED,
        Err(DisplayError::MissingDisplay | DisplayError::NoDisplays | DisplayError::Ambiguous) => {
            FR_INVALID
        }
        Err(
            DisplayError::CatalogChanged
            | DisplayError::StaleRequest
            | DisplayError::AlreadySelected,
        ) => FR_STALE,
    }
}

/// Submit once from this original approved catalog. The shared owner checks
/// the original `Arc` identity and full catalog again before accepting it.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_select_display(handle: FrHandle, token: u64, index: u32) -> i32 {
    let (session, request) = match current(handle, token) {
        Ok(original) => original,
        Err(error) => return error,
    };
    let Some(display) = usize::try_from(index)
        .ok()
        .and_then(|index| request.displays().get(index))
    else {
        return FR_INVALID;
    };
    status(session.shared().select_display(&request, display.handle))
}

/// A delayed dismissal cancels only its original pending request, never a
/// replacement attempt or a choice already accepted once.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_cancel_display(handle: FrHandle, token: u64) -> i32 {
    let (session, request) = match current(handle, token) {
        Ok(original) => original,
        Err(error) => return error,
    };
    status(session.shared().cancel_display(&request))
}

/// A replacement viewing session may start only after `FR_OK`: the original
/// service and cleanup succeeded following a local close. `FR_PENDING` means
/// still running; `FR_CLOSED` means finished without that proof. An end event
/// alone, including `FR_END_OK` after an interrupted dial, proves no cleanup.
#[unsafe(no_mangle)]
pub extern "C" fn fr_session_reopen_status(handle: FrHandle) -> i32 {
    let Some(session) = lookup(handle) else {
        return FR_STALE;
    };
    let shared = session.shared();
    if !shared.finished() {
        FR_PENDING
    } else if shared.orderly_closed() {
        FR_OK
    } else {
        FR_CLOSED
    }
}
