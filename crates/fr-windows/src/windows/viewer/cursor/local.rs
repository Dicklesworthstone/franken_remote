//! Apply frd's local cursor requests on the viewer's UI thread. The original
//! owner and request stay together through native work and acknowledgement.
use super::{super::control, native};
use crate::{cursor::Shape, cursor_image, geometry::Rect};
use fr_mobile_ffi::{
    local_cursor::{Error, Hub, Image, Owner, Pointer, Request, Scope},
    session::Shared,
};
use std::sync::Arc;
use windows::Win32::Foundation::HWND;

/// Only an actually presented picture supplies the native shape's geometry.
#[derive(Clone, Copy)]
pub(super) struct View {
    pub attempt: u64,
    pub picture: Option<(u64, u64, Rect)>,
    pub eligible: bool,
    pub overlay: bool,
    pub lifetime: u64,
}

impl View {
    fn rect(self) -> Option<Rect> {
        self.picture
            .filter(|(attempt, _, _)| *attempt == self.attempt)
            .map(|(_, _, dest)| dest)
    }

    fn shape_rect(self, scope: Scope) -> Option<Rect> {
        self.picture
            .filter(|(attempt, sequence, _)| {
                *attempt == self.attempt
                    && *attempt == scope.attempt
                    && *sequence > scope.frame_after
            })
            .map(|(_, _, dest)| dest)
    }
}

struct Installed {
    request: Request,
    cursor: Option<native::Cursor>,
    dest: Option<Rect>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    Managed,
    Unmanaged,
    /// The caller must erase the old overlay before returning the pointer
    /// and completing this original owner. No native work is done here.
    Retiring,
}

#[derive(Default)]
pub(in super::super) struct Local {
    owner: Option<Arc<Owner>>,
    scope: Option<Scope>,
    current: Option<Installed>,
    pending: Option<Request>,
    /// A failing temporary native destructor cannot be relabelled as reaped.
    /// Once set, admit no more allocation or successful owner completion.
    cleanup_unproven: bool,
}

fn contains(rect: Rect, point: (i32, i32)) -> bool {
    point.0 >= rect.left && point.0 < rect.right && point.1 >= rect.top && point.1 < rect.bottom
}

impl Local {
    fn clear_images(&mut self) -> Result<(), ()> {
        if let Some(current) = &mut self.current
            && let Some(cursor) = &mut current.cursor
        {
            cursor.retire()?;
        }
        self.current = None;
        self.pending = None;
        Ok(())
    }

    pub(super) fn unselect(&self) -> Result<(), ()> {
        if self
            .current
            .as_ref()
            .and_then(|current| current.cursor.as_ref())
            .is_some_and(|cursor| !cursor.unselect())
        {
            return Err(());
        }
        Ok(())
    }

    /// Terminal cleanup, unlike a temporary app-switch pause. Native resources
    /// are released before this exact capture attachment may report reaped.
    pub(super) fn retire(&mut self, hwnd: HWND) -> Result<(), ()> {
        if let Some(owner) = &self.owner {
            owner.stop();
        }
        self.clear_images()?;
        if native::point(hwnd).is_some() && !native::assign(false) {
            return Err(());
        }
        if self.cleanup_unproven {
            return Err(());
        }
        if let Some(owner) = self.owner.take() {
            owner.complete();
        }
        self.scope = None;
        Ok(())
    }

    /// Closing can precede the first UI poll of an attached owner. Collect
    /// that exact session's attachment too, after its original input fence.
    pub(super) fn retire_session(&mut self, shared: &Shared, hwnd: HWND) -> Result<(), ()> {
        self.retire(hwnd)?;
        let next = match shared.local_cursor().map(Hub::owner).transpose() {
            Ok(owner) => owner.flatten().filter(|owner| !owner.completed()),
            Err(Error::Busy) => return Ok(()),
            Err(_) => return Err(()),
        };
        if let Some(owner) = next
            && !owner.completed()
        {
            self.owner = Some(owner);
            self.retire(hwnd)?;
        }
        Ok(())
    }

    /// This also runs AFTER selecting a decoded sample and BEFORE Present.
    /// Old native images cannot survive a decoder reset onto a newer image.
    pub(super) fn synchronize(&mut self, shared: &Shared, attempt: u64) -> Result<Status, ()> {
        if self.cleanup_unproven {
            return Err(());
        }
        let next = match shared.local_cursor().map(Hub::owner).transpose() {
            Ok(owner) => owner.flatten().filter(|owner| !owner.completed()),
            Err(Error::Busy) => {
                self.unselect()?;
                return Ok(Status::Unmanaged);
            }
            Err(_) => return Err(()),
        };
        if self
            .owner
            .as_ref()
            .zip(next.as_ref())
            .is_none_or(|(old, new)| !Arc::ptr_eq(old, new))
        {
            if self.owner.is_some() {
                return Ok(Status::Retiring);
            }
            self.owner = next;
        }
        let Some(owner) = self.owner.as_ref() else {
            return Ok(Status::Unmanaged);
        };
        if owner.stopped() {
            return Ok(Status::Retiring);
        }
        let scope = match owner.scope() {
            Ok(scope) => scope,
            Err(Error::Busy) => {
                self.unselect()?;
                return Ok(Status::Unmanaged);
            }
            Err(_) => return Err(()),
        };
        if scope.attempt != attempt || attempt == 0 {
            return Ok(Status::Retiring);
        }
        if self.scope != Some(scope) {
            self.clear_images()?;
            self.scope = Some(scope);
        }
        Ok(Status::Managed)
    }

    fn create(&mut self, pixels: &cursor_image::Image) -> Result<native::Cursor, ()> {
        native::Cursor::new(pixels).map_err(|()| {
            // Construction can fail after creating native temporaries. Its
            // best-effort Drop is not positive cleanup evidence.
            self.cleanup_unproven = true;
        })
    }

    fn discard(&mut self, mut cursor: native::Cursor) -> Result<(), ()> {
        cursor.retire().map_err(|()| {
            self.cleanup_unproven = true;
        })
    }

    fn install(&mut self, owner: &Owner, request: Request, dest: Option<Rect>) -> Result<(), ()> {
        let cursor = if let Image::Shape(shape) = &request.image {
            let Some(dest) = dest else {
                self.pending = Some(request);
                return Ok(());
            };
            let (width, height, hotspot_x, hotspot_y) = shape.geometry();
            let pixels = cursor_image::prepare(
                Shape {
                    width,
                    height,
                    hotspot_x,
                    hotspot_y,
                },
                shape.rgba(),
                owner.display(),
                dest,
            )
            .map_err(|_| ())?;
            Some(self.create(&pixels)?)
        } else {
            None
        };
        // Native preparation may dispatch callbacks or race a reset. Retain
        // neither a false acknowledgement nor an image of the retired scope.
        let current = owner.current(&request.stamp);
        match current {
            Ok(true) => {}
            Ok(false) | Err(Error::Busy) => {
                if let Some(cursor) = cursor {
                    self.discard(cursor)?;
                }
                // A lock wait is not retirement. The core still waits for
                // this exact generation unless a newer request supersedes it.
                if current == Err(Error::Busy) {
                    self.pending = Some(request);
                }
                return Ok(());
            }
            Err(_) => {
                if let Some(cursor) = cursor {
                    self.discard(cursor)?;
                }
                return Err(());
            }
        }
        if self.clear_images().is_err() {
            if let Some(cursor) = cursor {
                self.discard(cursor)?;
            }
            return Err(());
        }
        self.current = Some(Installed {
            request,
            cursor,
            dest,
        });
        Ok(())
    }

    /// Report whether this live grant manages the native pointer. No effect
    /// is credited while preparing a shape or showing an inactive fallback.
    #[allow(
        clippy::too_many_lines,
        reason = "one ordered GUI request and acknowledgement sequence"
    )]
    pub(super) fn refresh(
        &mut self,
        shared: &Shared,
        view: View,
        hwnd: HWND,
    ) -> Result<Status, ()> {
        let status = self.synchronize(shared, view.attempt)?;
        if status != Status::Managed {
            return Ok(status);
        }
        let Some(owner) = self.owner.clone() else {
            return Ok(Status::Unmanaged);
        };
        let point = native::point(hwnd);
        let within = point
            .zip(view.rect())
            .is_some_and(|(point, rect)| contains(rect, point));
        let inside = view.eligible && shared.active() && native::foreground(hwnd) && within;
        owner.observe(if inside {
            Pointer::Inside
        } else {
            Pointer::Outside
        });
        match owner.take() {
            Ok(Some(request)) => self.pending = Some(request),
            Ok(None) => {}
            Err(Error::Busy) => {
                self.unselect()?;
                return Ok(Status::Unmanaged);
            }
            Err(Error::Stopped) => {
                return Ok(Status::Retiring);
            }
            Err(Error::Invalid) => return Err(()),
        }
        if let Some(request) = self.pending.take() {
            let draws = !matches!(request.image, Image::Blank);
            let shape = matches!(request.image, Image::Shape(_));
            if (draws && view.overlay) || (shape && !inside) {
                self.pending = Some(request);
            } else {
                let dest = view.shape_rect(request.stamp.scope());
                self.install(&owner, request, dest)?;
            }
        }

        // Resize can change the native cursor scale without changing its host
        // shape. Prepare only one replacement; pointer motion allocates none.
        let resize = self.current.as_ref().and_then(|current| {
            let dest = view.shape_rect(current.request.stamp.scope())?;
            (matches!(current.request.image, Image::Shape(_)) && current.dest != Some(dest))
                .then_some(dest)
        });
        if let Some(dest) = resize
            && inside
            && !view.overlay
        {
            // Borrow the current image throughout preparation: every failure
            // retains its original native handle for explicit cleanup.
            let Some(current) = &self.current else {
                return Err(());
            };
            let Image::Shape(shape) = &current.request.image else {
                return Err(());
            };
            let (width, height, hotspot_x, hotspot_y) = shape.geometry();
            let pixels = cursor_image::prepare(
                Shape {
                    width,
                    height,
                    hotspot_x,
                    hotspot_y,
                },
                shape.rgba(),
                owner.display(),
                dest,
            )
            .map_err(|_| ())?;
            let replacement = self.create(&pixels)?;
            let valid = self
                .current
                .as_ref()
                .is_some_and(|current| owner.current(&current.request.stamp) == Ok(true));
            if valid {
                if let Some(current) = &mut self.current {
                    if let Some(cursor) = &mut current.cursor
                        && cursor.retire().is_err()
                    {
                        self.discard(replacement)?;
                        return Err(());
                    }
                    current.cursor = Some(replacement);
                    current.dest = Some(dest);
                }
            } else {
                self.discard(replacement)?;
            }
        }

        let point = native::point(hwnd);
        let within = point
            .zip(view.rect())
            .is_some_and(|(point, rect)| contains(rect, point));
        let inside = view.eligible
            && shared.active()
            && within
            && native::foreground(hwnd)
            && view.lifetime == control::lifetime()
            && view.attempt == shared.frames.app_id(0)
            && !owner.stopped();
        owner.observe(if inside {
            Pointer::Inside
        } else {
            Pointer::Outside
        });
        let Some(current) = &self.current else {
            if point.is_some() && !native::assign(inside && view.overlay) {
                return Err(());
            }
            return Ok(Status::Managed);
        };
        if owner.current(&current.request.stamp) != Ok(true) {
            self.unselect()?;
            return Ok(Status::Unmanaged);
        }
        let applied = if inside && view.overlay {
            if !native::assign(true) {
                return Err(());
            }
            matches!(current.request.image, Image::Blank)
        } else if inside {
            match &current.request.image {
                Image::Default => native::assign(false),
                Image::Blank => native::assign(true),
                Image::Shape(_) => {
                    view.shape_rect(current.request.stamp.scope()).is_some()
                        && current.cursor.as_ref().is_some_and(native::Cursor::assign)
                }
            }
        } else {
            // A pointer in another window/letterbox is not a renderer of the
            // remote cursor. Keep the managed image for the next eligible
            // crossing, without changing another window's pointer.
            self.unselect()?;
            if point.is_some() && !native::assign(false) {
                return Err(());
            }
            match current.request.image {
                Image::Default => true,
                Image::Blank => !within,
                Image::Shape(_) => false,
            }
        };
        if applied {
            // Recheck the original capture AFTER native calls. A background
            // fallback never acknowledges a host shape as locally installed.
            let shape = matches!(current.request.image, Image::Shape(_));
            if !shape
                || (shared.active()
                    && native::foreground(hwnd)
                    && view.lifetime == control::lifetime())
            {
                match owner.acknowledge(&current.request.stamp) {
                    Ok(true) => {}
                    Ok(false) | Err(Error::Busy) => {
                        self.unselect()?;
                    }
                    Err(_) => return Err(()),
                }
            } else {
                self.unselect()?;
            }
        }
        Ok(Status::Managed)
    }
}
