//! Copy the shared cursor without holding its mutex across native work.
//! The remote overlay and the local pointer hand off in presentation order:
//! blank before showing an overlay, restore only after erasing it.
mod local;
mod native;
use super::{Viewer, control};
use crate::{
    change_display::Refusal,
    cursor::{self, Shape, State},
    geometry::Rect,
};
pub(super) use local::Local;
use std::sync::TryLockError;
use windows::Win32::Foundation::HWND;

/// Returns whether the original shared snapshot changed. One shape copy at
/// most; cursor movement does not allocate or upload another bitmap.
pub(super) fn synchronize(viewer: &mut Viewer, hwnd: HWND) -> Result<bool, ()> {
    let shared = viewer.session.shared_handle();
    let attempt = shared.frames.app_id(0);
    let old = viewer.renderer.cursor_state();
    let mut changed = false;
    if old.attempt != 0 && old.attempt != attempt {
        // Internal retries can reuse Shared. Erase the original attempt's
        // visible pixels before its native pointer ownership is released.
        // On failure keep cursor_drawn so terminal retirement still knows
        // that the old overlay may remain on screen.
        erase(viewer, hwnd)?;
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
        && viewer.session.shared().active()
        && !viewer.session.shared().finished()
        && viewer.session.shared().frames.app_id(0) == attempt
        && cursor::hide_local(viewer.session.shared().controlled(), true, dest, point)
}

/// Prevent a double remote cursor on the first frame of a new overlay.
/// An old overlay is erased before `refresh` may show the local arrow again.
pub(super) fn before_draw(
    viewer: &mut Viewer,
    hwnd: HWND,
    overlay: Option<(u64, Rect)>,
) -> Result<(), ()> {
    let shared = viewer.session.shared_handle();
    if viewer
        .local_cursor
        .synchronize(&shared, shared.frames.app_id(0))?
        == local::Status::Retiring
    {
        erase(viewer, hwnd)?;
        viewer.local_cursor.retire(hwnd)?;
    }
    if terminal(viewer) {
        return Err(());
    }
    if viewer.picture_drawn.is_none() {
        viewer.local_cursor.unselect()?;
    }
    if let Some(point) = native::point(hwnd)
        && native::foreground(hwnd)
        && should_hide(viewer, overlay, point)
        && !native::assign(true)
    {
        return Err(());
    }
    Ok(())
}

/// Refresh stationary pointers as well as `WM_SETCURSOR` transitions. Every
/// native request is handled, including restoring a previously owned shape.
pub(super) fn refresh(viewer: &mut Viewer, hwnd: HWND) {
    // A worker or native callback can retire the owner between UI ticks.
    // Erase its already presented overlay before restoring the local arrow.
    if terminal(viewer)
        || viewer
            .cursor_drawn
            .is_some_and(|(attempt, _)| viewer.session.shared().frames.app_id(0) != attempt)
    {
        retire(viewer, hwnd);
        return;
    }
    let shared = viewer.session.shared_handle();
    let view = local::View {
        attempt: shared.frames.app_id(0),
        picture: viewer.picture_drawn,
        eligible: viewer.text.is_none()
            && viewer.display_picker.is_none()
            && !viewer.choosing_file()
            && control::visible_now()
            && shared.active()
            && shared.controlled(),
        overlay: viewer.cursor_drawn.is_some(),
        lifetime: control::lifetime(),
    };
    match viewer.local_cursor.refresh(&shared, view, hwnd) {
        Ok(local::Status::Managed) => return,
        Ok(local::Status::Unmanaged) => {}
        Ok(local::Status::Retiring) => {
            if erase(viewer, hwnd)
                .and_then(|()| viewer.local_cursor.retire(hwnd))
                .is_err()
            {
                fail(viewer, hwnd);
                return;
            }
        }
        Err(()) => {
            fail(viewer, hwnd);
            retire(viewer, hwnd);
            return;
        }
    }
    let Some(point) = native::point(hwnd) else {
        return;
    };
    let hide = native::foreground(hwnd) && should_hide(viewer, viewer.cursor_drawn, point);
    if !native::assign(hide) {
        fail(viewer, hwnd);
    }
}

fn terminal(viewer: &Viewer) -> bool {
    viewer.closing || viewer.reopen.pending() || viewer.session.shared().finished()
}

/// Fence first. A failed clear must not leave old pixels visible while native
/// ownership returns. Failure to confirm hiding also prevents completion.
fn fail(viewer: &mut Viewer, hwnd: HWND) -> bool {
    viewer.session.shared().presentation_failed();
    viewer.reopen_refusal.get_or_insert(Refusal::Presentation);
    viewer.close();
    control::visible(false);
    native::hide_failed_view(hwnd)
}

fn erase(viewer: &mut Viewer, hwnd: HWND) -> Result<(), ()> {
    if viewer.cursor_drawn.is_some()
        || viewer.picture_drawn.is_some()
        || viewer.renderer.cursor_state().visible
    {
        let accepted = viewer.renderer.clear().is_ok() && viewer.renderer.present_accepted();
        if accepted {
            viewer.cursor_drawn = None;
            viewer.picture_drawn = None;
        } else if !fail(viewer, hwnd) {
            return Err(());
        }
        // Keep an unaccepted overlay witness until it is erased or the
        // original window is destroyed. A hidden failure grants no freshness.
        viewer.repaint_pending = true;
    }
    viewer.renderer.clear_cursor();
    Ok(())
}

pub(super) fn retire(viewer: &mut Viewer, hwnd: HWND) {
    if erase(viewer, hwnd).is_err() {
        return;
    }
    let shared = viewer.session.shared_handle();
    let retired = if terminal(viewer) {
        viewer.local_cursor.retire_session(&shared, hwnd)
    } else {
        // A stale selected frame can lose a race to an internal retry. It
        // cannot stop the replacement attempt's already-current native owner.
        match viewer
            .local_cursor
            .synchronize(&shared, shared.frames.app_id(0))
        {
            Ok(local::Status::Retiring) => viewer.local_cursor.retire(hwnd),
            Ok(_) => Ok(()),
            Err(()) => Err(()),
        }
    };
    if retired.is_err() {
        fail(viewer, hwnd);
        return;
    }
    if native::point(hwnd).is_some() && !native::assign(false) {
        fail(viewer, hwnd);
    }
}
