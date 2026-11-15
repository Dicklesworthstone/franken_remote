//! Thread-local pointer access, without changing the global `ShowCursor` count
//! or replacing any shared system cursor image.
use windows::Win32::{
    Foundation::{HWND, POINT},
    Graphics::Gdi::ScreenToClient,
    UI::WindowsAndMessaging::{
        GetCursorPos, IDC_ARROW, LoadCursorW, SW_HIDE, SetCursor, ShowWindow, WindowFromPoint,
    },
};

pub(super) fn point(hwnd: HWND) -> Option<(i32, i32)> {
    let mut point = POINT::default();
    // SAFETY: a writable local point and this UI thread's live HWND. Never
    // replace another window's cursor, even if it overlaps our picture.
    unsafe {
        GetCursorPos(&raw mut point).ok()?;
        if WindowFromPoint(point) != hwnd || !ScreenToClient(hwnd, &raw mut point).as_bool() {
            return None;
        }
    }
    Some((point.x, point.y))
}

/// Apply only after proving the pointer belongs to the viewer's own HWND.
/// A failed stock-cursor load leaves the last assignment unchanged.
pub(super) fn assign(hidden: bool) -> bool {
    // SAFETY: stock cursors are shared OS resources, never destroyed by us.
    // SetCursor only changes this UI thread's current pointer image.
    unsafe {
        if hidden {
            SetCursor(None);
        } else {
            let Ok(arrow) = LoadCursorW(None, IDC_ARROW) else {
                return false;
            };
            SetCursor(Some(arrow));
        }
    }
    true
}

/// A failed renderer cannot leave a stale overlay visible during cleanup.
pub(super) fn hide_failed_view(hwnd: HWND) {
    // SAFETY: this UI thread owns the live viewer window. The caller has
    // already fenced input and permanently closed its session before hiding.
    let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
}
