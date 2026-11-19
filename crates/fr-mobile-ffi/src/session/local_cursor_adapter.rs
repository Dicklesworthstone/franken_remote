//! Adapt the bounded UI exchange to frd's existing single-cursor owner.
use crate::local_cursor::{self, Image, Owner, Pointer, Shape};
use frd::session_startup::{
    ViewerControl,
    viewer_local_cursor::{self as cursor, LocalCursor},
};
use std::sync::Arc;

pub(super) struct Handle {
    pub owner: Arc<Owner>,
    pub control: ViewerControl,
}

impl Handle {
    fn stopped(&self) -> bool {
        if self.control.is_stopped() {
            self.owner.stop();
        }
        self.owner.stopped()
    }
}

impl LocalCursor for Handle {
    fn state(&self) -> cursor::State {
        cursor::State {
            pointer: match self.owner.pointer() {
                Pointer::Unknown => cursor::LocalPointer::Unknown,
                Pointer::Inside => cursor::LocalPointer::Inside,
                Pointer::Outside => cursor::LocalPointer::Outside,
            },
            applied: self.owner.applied(),
            stopped: self.stopped(),
        }
    }

    fn request(&mut self, image: cursor::Image<'_>) -> Result<u64, cursor::Refused> {
        if self.stopped() {
            return Err(cursor::Refused::Stopped);
        }
        self.owner
            .request(|| {
                Ok(match image {
                    cursor::Image::Default => Image::Default,
                    cursor::Image::Blank => Image::Blank,
                    cursor::Image::Shape {
                        width,
                        height,
                        hotspot_x,
                        hotspot_y,
                        rgba,
                    } => Image::Shape(Shape::new(width, height, hotspot_x, hotspot_y, rgba)?),
                })
            })
            .map_err(|error| match error {
                local_cursor::Error::Busy => cursor::Refused::Busy,
                local_cursor::Error::Invalid => cursor::Refused::Invalid,
                local_cursor::Error::Stopped => cursor::Refused::Stopped,
            })
    }

    fn stop(&self) {
        self.owner.stop();
    }
}
