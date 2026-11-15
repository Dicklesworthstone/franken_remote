//! Copy the shared cursor without holding its mutex across native work.
//! The remote overlay and the local pointer hand off in presentation order:
//! blank before showing an overlay, restore only after erasing it.
mod native;
use super::{Viewer, control};
use crate::{
    change_display::Refusal,
    cursor::{self, Shape, State},
    geometry::Rect,
};
use std::sync::TryLockError;
use windows::Win32::Foundation::HWND;

/// Returns whether the original shared snapshot changed. One shape copy at
/// most; cursor movement does not allocate or upload another bitmap.
pub(super) fn synchronize(viewer: &mut Viewer) -> Result<bool, ()> {
    let shared = viewer.session.shared();
    let attempt = shared.frames.app_id(0);
    let old = viewer.renderer.cursor_state();
    let mut changed = false;
    if old.attempt != 0 && old.attempt != attempt {
        // Internal retries can reuse Shared. Erase the original attempt's
        // visible pixels before its native pointer ownership is released.
        // On failure keep cursor_drawn so terminal retirement still knows
        // that the old overlay may remain on screen.
        viewer.renderer.clear().map_err(|_| ())?;
        viewer.cursor_drawn = None;
        changed = true;
    }
    let captured = {
        let cursor = match shared.cursor.try_lock() {
            Ok(cursor) => cursor,
            Err(TryLockError::WouldBlock) => {
                // A configuration reset may own the mutex. Do not carry its
                // predecessor's cached cursor onto a newly selected picture.
                // Keep cursor_drawn until the repaint erases actual pixels.
                viewer.renderer.clear_cursor();
                return Ok(true);
            }
            Err(TryLockError::Poisoned(_)) => return Err(()),
        };
        if cursor.attempt == 0 || cursor.attempt != attempt {
            return Ok(changed);
        }
        let state = State {
            attempt: cursor.attempt,
            generation: cursor.generation,
            shape_generation: cursor.shape_generation,
            frame_after: cursor.frame_after,
            source: (cursor.source_width, cursor.source_height),
            visible: cursor.visible,
            x: cursor.x,
            y: cursor.y,
            shape: Shape {
                width: cursor.width,
                height: cursor.height,
                hotspot_x: cursor.hotspot_x,
                hotspot_y: cursor.hotspot_y,
            },
        };
        if state == old {
            return Ok(changed);
        }
        let pixels = if state.shape.bytes().is_some() && state.shape_key() != old.shape_key() {
            Some(cursor::premultiplied(state.shape, &cursor.rgba).map_err(|_| ())?)
        } else {
            None
        };
        (state, pixels)
    };
    // No shared cursor/input/authority lock remains across COM or allocation.
    viewer
        .renderer
        .update_cursor(captured.0, captured.1.as_deref())
        .map_err(|_| ())?;
    Ok(true)
}

fn should_hide(viewer: &Viewer, overlay: Option<(u64, Rect)>, point: (i32, i32)) -> bool {
    let Some((attempt, dest)) = overlay else {
        return false;
    };
    !viewer.closing
        && !viewer.reopen.pending()
        && viewer.text.is_none()
        && viewer.display_picker.is_none()
        && !viewer.choosing_file()
        && control::visible_now()
        && !viewer.session.shared().finished()
        && viewer.session.shared().frames.app_id(0) == attempt
        && cursor::hide_local(viewer.session.shared().controlled(), true, dest, point)
}

/// Prevent a double remote cursor on the first frame of a new overlay.
/// An old overlay is erased before `refresh` may show the local arrow again.
pub(super) fn before_draw(viewer: &mut Viewer, hwnd: HWND, overlay: Option<(u64, Rect)>) {
    if let Some(point) = native::point(hwnd)
        && should_hide(viewer, overlay, point)
        && native::assign(true)
    {
        viewer.local_cursor_hidden = true;
    }
}

/// Refresh stationary pointers as well as `WM_SETCURSOR` transitions. `force`
/// responds to the native request even when our last assignment was equal.
pub(super) fn refresh(viewer: &mut Viewer, hwnd: HWND, force: bool) {
    // A worker or native callback can retire the owner between UI ticks.
    // Erase its already presented overlay before restoring the local arrow.
    if viewer.cursor_drawn.is_some_and(|(attempt, _)| {
        viewer.closing
            || viewer.reopen.pending()
            || viewer.session.shared().finished()
            || viewer.session.shared().frames.app_id(0) != attempt
    }) {
        retire(viewer, hwnd);
        return;
    }
    let Some(point) = native::point(hwnd) else {
        viewer.local_cursor_hidden = false;
        return;
    };
    let hide = should_hide(viewer, viewer.cursor_drawn, point);
    if (force || hide != viewer.local_cursor_hidden) && native::assign(hide) {
        viewer.local_cursor_hidden = hide;
    }
}

pub(super) fn retire(viewer: &mut Viewer, hwnd: HWND) {
    // Dropping a cached bitmap does not erase pixels already in the swap
    // chain. Erase once before restoring the local pointer, including a draw
    // whose failing Present left its actual display outcome unknown.
    let erase = viewer.cursor_drawn.take().is_some() || viewer.renderer.cursor_state().visible;
    if erase && viewer.renderer.clear().is_err() {
        viewer.session.shared().presentation_failed();
        viewer.reopen_refusal.get_or_insert(Refusal::Presentation);
        viewer.close();
        control::visible(false);
        // A failed surface cannot prove its old overlay was erased. Hide it
        // before handing cursor ownership back; cleanup keeps its old owner.
        native::hide_failed_view(hwnd);
    }
    viewer.renderer.clear_cursor();
    refresh(viewer, hwnd, false);
}
