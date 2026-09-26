//! The viewer's single-owner wiring over the real bounded tracker, with a
//! scripted platform owner (no native window here; the namespace e2e drives
//! the real X11 owner). Every overlay step is acknowledged like the presenter.
use super::*;
use crate::session_startup::viewer::controlled::local_cursor::{LocalPointer, State};
use fr_core::ids::DisplayGeometryGeneration;
use fr_wire::cursor::{CursorPosition, CursorShape, POSITION_FLAG_VISIBLE, SHAPE_FLAG_VISIBLE};

const MAGENTA: [u8; 4] = [0xff, 0x00, 0xff, 0xff];
/// No overlay step was taken.
const NONE: [Option<At>; 0] = [];

/// (Debug text, shape dimensions and pixels) of each admitted request.
type Request = (String, Option<(u16, u16, Vec<u8>)>);
/// A platform owner that records requests and acknowledges on demand.
#[derive(Default)]
struct Platform {
    pointer: Option<LocalPointer>,
    generation: u64,
    applied: u64,
    stopped: bool,
    busy: bool,
    requests: Vec<Request>,
}
impl LocalCursor for Platform {
    fn state(&self) -> State {
        State {
            pointer: self.pointer.unwrap_or(LocalPointer::Unknown),
            applied: self.applied,
            stopped: self.stopped,
        }
    }
    fn request(&mut self, image: Image<'_>) -> Result<u64, Refused> {
        if self.stopped {
            return Err(Refused::Stopped);
        }
        if self.busy {
            return Err(Refused::Busy);
        }
        self.generation += 1;
        let pixels = match image {
            Image::Shape {
                width,
                height,
                rgba,
                ..
            } => Some((width, height, rgba.to_vec())),
            Image::Default | Image::Blank => None,
        };
        self.requests.push((format!("{image:?}"), pixels));
        Ok(self.generation)
    }
    fn stop(&self) {}
}
impl Platform {
    fn ack(&mut self) {
        self.applied = self.generation;
    }
    fn last(&self) -> &str {
        &self.requests.last().expect("a request").0
    }
}

fn remote() -> Remote {
    let mut remote = Remote::new(DisplayGeometryGeneration::INITIAL);
    let rgba: Vec<u8> = MAGENTA.repeat(16 * 16);
    remote
        .tracker
        .store_shape(&CursorShape {
            shape_id: 5,
            width: 16,
            height: 16,
            hotspot_x: 0,
            hotspot_y: 0,
            scale_1000: 1000,
            flags: SHAPE_FLAG_VISIBLE,
            rgba: &rgba,
        })
        .unwrap();
    remote
}
fn at(remote: &mut Remote, shape: u32, x: i32, y: i32, visible: bool, sequence: u64) {
    remote
        .tracker
        .process_position(&CursorPosition {
            shape_id: shape,
            x,
            y,
            geometry_generation: DisplayGeometryGeneration::INITIAL.as_raw(),
            sequence,
            flags: if visible { POSITION_FLAG_VISIBLE } else { 0 },
        })
        .unwrap();
}
/// One call's worth of steps with the presenter acknowledging each overlay.
fn run(
    remote: &mut Remote,
    controlling: bool,
    platform: Option<&mut Platform>,
    history: Option<&PointerHistory>,
) -> Vec<Option<At>> {
    let mut local = platform.map(|p| p as &mut (dyn LocalCursor + 'static));
    let mut overlays = Vec::new();
    for _ in 0..MAX_STEPS {
        let Step::Overlay(overlay) = remote.advance(controlling, local.as_deref_mut(), history, 10)
        else {
            break;
        };
        assert!(remote.overlay_update(overlay).is_some());
        remote.overlay_applied(overlay);
        overlays.push(overlay);
    }
    overlays
}

#[test]
fn observing_composites_the_overlay_and_never_touches_the_local_pointer() {
    let mut remote = remote();
    at(&mut remote, 5, 200, 150, true, 1);
    let overlays = run(&mut remote, false, None, None);
    assert_eq!(
        overlays,
        [Some(At {
            shape: 5,
            x: 200,
            y: 150
        })]
    );
    // The first overlay carries the image; the next move does not re-send it.
    at(&mut remote, 5, 210, 150, true, 2);
    let target = Some(At {
        shape: 5,
        x: 210,
        y: 150,
    });
    assert!(matches!(
        remote.overlay_update(target),
        Some(Update::Move { x: 210, y: 150 })
    ));
}

#[test]
fn a_followed_host_pointer_is_drawn_once_as_the_local_pointer_with_the_remote_shape() {
    let mut remote = remote();
    let mut platform = Platform {
        pointer: Some(LocalPointer::Inside),
        ..Platform::default()
    };
    let mut history = PointerHistory::default();
    history.submitted(200, 150, 5);
    at(&mut remote, 5, 200, 150, true, 1);
    let overlays = run(&mut remote, true, Some(&mut platform), Some(&history));
    assert_eq!(overlays, NONE, "an overlay beside the local pointer");
    assert_eq!(platform.requests.len(), 1);
    let (_, pixels) = &platform.requests[0];
    let (width, height, rgba) = pixels.as_ref().expect("the confirmed shape");
    assert_eq!((*width, *height), (16, 16));
    assert!(rgba.chunks(4).all(|p| p == MAGENTA));
    platform.ack();
    assert_eq!(
        run(&mut remote, true, Some(&mut platform), Some(&history)),
        NONE
    );
    assert_eq!(platform.requests.len(), 1, "no churn once applied");
}

#[test]
fn a_host_moved_pointer_blanks_the_local_pointer_before_the_overlay_appears() {
    let mut remote = remote();
    let mut platform = Platform {
        pointer: Some(LocalPointer::Inside),
        ..Platform::default()
    };
    let mut history = PointerHistory::default();
    history.submitted(200, 150, 5);
    at(&mut remote, 5, 200, 150, true, 1);
    run(&mut remote, true, Some(&mut platform), Some(&history));
    platform.ack();
    // The host's own pointer moved somewhere this client never sent it.
    at(&mut remote, 5, 420, 300, true, 2);
    assert_eq!(
        run(&mut remote, true, Some(&mut platform), Some(&history)),
        NONE
    );
    assert_eq!(platform.last(), "Blank");
    // Not yet acknowledged: still no overlay.
    assert_eq!(
        run(&mut remote, true, Some(&mut platform), Some(&history)),
        NONE
    );
    platform.ack();
    assert_eq!(
        run(&mut remote, true, Some(&mut platform), Some(&history)),
        [Some(At {
            shape: 5,
            x: 420,
            y: 300
        })]
    );
    // The local pointer leaves the window: the overlay stays the one owner.
    platform.pointer = Some(LocalPointer::Outside);
    history.submitted(420, 300, 9);
    assert_eq!(
        run(&mut remote, true, Some(&mut platform), Some(&history)),
        NONE
    );
    // Back inside where the host now follows: overlay off, THEN the shape.
    platform.pointer = Some(LocalPointer::Inside);
    let requests = platform.requests.len();
    assert_eq!(
        run(&mut remote, true, Some(&mut platform), Some(&history)),
        [None]
    );
    assert_eq!(platform.requests.len(), requests + 1);
    assert!(platform.last().starts_with("Shape"));
}

#[test]
fn an_unknown_shape_is_the_fallback_and_hidden_draws_nothing() {
    let mut remote = remote();
    let mut platform = Platform {
        pointer: Some(LocalPointer::Inside),
        ..Platform::default()
    };
    let mut history = PointerHistory::default();
    history.submitted(10, 10, 5);
    // A position naming a shape that has not arrived (never allocated).
    at(&mut remote, 99, 10, 10, true, 1);
    assert_eq!(
        run(&mut remote, true, Some(&mut platform), Some(&history)),
        NONE
    );
    let (_, pixels) = &platform.requests[0];
    let (width, height, _) = pixels.as_ref().unwrap();
    assert_eq!((*width, *height), (7, 7), "the built-in fallback crosshair");
    platform.ack();
    at(&mut remote, 5, 10, 10, false, 2);
    assert_eq!(
        run(&mut remote, true, Some(&mut platform), Some(&history)),
        NONE
    );
    assert_eq!(platform.last(), "Blank");
    platform.ack();
    assert_eq!(
        run(&mut remote, true, Some(&mut platform), Some(&history)),
        NONE
    );
}

#[test]
fn a_busy_owner_is_retried_and_a_stopped_owner_is_no_longer_managed() {
    let mut remote = remote();
    let mut platform = Platform {
        pointer: Some(LocalPointer::Inside),
        busy: true,
        ..Platform::default()
    };
    let history = PointerHistory::default();
    at(&mut remote, 5, 300, 200, true, 1);
    assert_eq!(
        run(&mut remote, true, Some(&mut platform), Some(&history)),
        NONE
    );
    assert_eq!(platform.requests.len(), 0);
    platform.busy = false;
    run(&mut remote, true, Some(&mut platform), Some(&history));
    assert_eq!(platform.last(), "Blank");
    // The owner stops: unmanaged, an unexplained host position is still shown.
    platform.stopped = true;
    assert_eq!(
        run(&mut remote, true, Some(&mut platform), Some(&history)),
        [Some(At {
            shape: 5,
            x: 300,
            y: 200
        })]
    );
}
