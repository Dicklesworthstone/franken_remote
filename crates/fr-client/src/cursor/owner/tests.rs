use super::*;

const FALLBACK: u32 = 0;

fn visible(shape: u32, x: i32, y: i32) -> Confirmed {
    Confirmed::Visible { shape, x, y }
}
fn controlling(local: LocalPointer, confirmed: Confirmed, explained: bool) -> Target {
    resolve(Inputs {
        controlling: true,
        window: Some(local),
        confirmed,
        explained,
    })
}

#[test]
fn history_is_bounded_and_explains_only_this_clients_own_submissions() {
    let mut history = PointerHistory::default();
    assert!(history.is_empty());
    assert!(
        !history.explains(0, 0, 0),
        "nothing submitted explains nothing"
    );
    for i in 0..100 {
        history.submitted(i, 2 * i, 1_000 + u64::try_from(i).unwrap());
    }
    assert_eq!(history.len(), HISTORY_POSITIONS);
    // The 64 newest are retained; older ones were overwritten, never grown.
    assert!(history.explains(99, 198, 2_000));
    assert!(history.explains(36, 72, 2_000));
    assert!(!history.explains(35, 70, 2_000));
    // A position this client never sent (the host moved on its own).
    assert!(!history.explains(99, 199, 2_000));
    // Age: only the LATEST survives the horizon (an idle pointer stays put).
    let later = 1_099 + HISTORY_US + 1;
    assert!(history.explains(99, 198, later));
    assert!(!history.explains(98, 196, later));
    assert!(history.explains(98, 196, 1_098 + HISTORY_US));
    // A clock that went backwards never explains by underflow.
    assert!(!history.explains(98, 196, 0));
    history.clear();
    assert!(!history.explains(99, 198, 2_000));
    // No coordinates reach diagnostics.
    history.submitted(1234, 5678, 1);
    let debug = format!("{history:?}");
    assert!(
        !debug.contains("1234") && !debug.contains("5678"),
        "{debug}"
    );
}

#[test]
fn every_resolution_while_controlling_has_at_most_one_drawn_owner() {
    let locals = [
        LocalPointer::Unknown,
        LocalPointer::Inside,
        LocalPointer::Outside,
    ];
    let confirmed = [
        Confirmed::Unknown,
        Confirmed::Hidden,
        visible(7, 10, 20),
        visible(FALLBACK, 10, 20),
    ];
    for local in locals {
        for c in confirmed {
            for explained in [false, true] {
                let t = controlling(local, c, explained);
                assert!(t.exclusive);
                let window = t.window.expect("a managed platform pointer");
                assert!(
                    !(t.overlay.is_some() && window.draws()),
                    "{local:?} {c:?} {explained}: {t:?}"
                );
            }
        }
    }
}

#[test]
fn the_local_pointer_carries_the_confirmed_shape_while_the_host_follows_it() {
    let t = controlling(LocalPointer::Inside, visible(7, 10, 20), true);
    assert_eq!(t.overlay, None);
    assert_eq!(t.window, Some(WindowCursor::Shape(7)));
    // Not yet observed crossing: the local pointer may be over the window.
    let t = controlling(LocalPointer::Unknown, visible(7, 10, 20), true);
    assert_eq!(t.window, Some(WindowCursor::Shape(7)));
}

#[test]
fn an_unknown_shape_uses_the_fallback_image_never_a_guess() {
    // The bounded cache resolves an unknown ID to the built-in fallback.
    let t = controlling(LocalPointer::Inside, visible(FALLBACK, 1, 1), true);
    assert_eq!(t.window, Some(WindowCursor::Shape(FALLBACK)));
    let t = controlling(LocalPointer::Inside, visible(FALLBACK, 1, 1), false);
    assert_eq!(t.overlay.map(|a| a.shape), Some(FALLBACK));
}

#[test]
fn a_host_moved_pointer_or_a_departed_local_pointer_hands_rendering_to_the_overlay() {
    let at = At {
        shape: 7,
        x: 10,
        y: 20,
    };
    for (local, explained) in [
        (LocalPointer::Inside, false),
        (LocalPointer::Unknown, false),
        (LocalPointer::Outside, true),
        (LocalPointer::Outside, false),
    ] {
        let t = controlling(local, visible(7, 10, 20), explained);
        assert_eq!(t.overlay, Some(at), "{local:?} {explained}");
        assert_eq!(t.window, Some(WindowCursor::Blank), "{local:?} {explained}");
    }
}

#[test]
fn hidden_means_no_client_cursor_and_nothing_confirmed_leaves_the_platform_pointer() {
    for local in [LocalPointer::Inside, LocalPointer::Outside] {
        for explained in [false, true] {
            let t = controlling(local, Confirmed::Hidden, explained);
            assert_eq!(t.overlay, None);
            assert_eq!(t.window, Some(WindowCursor::Blank));
            let t = controlling(local, Confirmed::Unknown, explained);
            assert_eq!(t.overlay, None);
            assert_eq!(t.window, Some(WindowCursor::Default));
        }
    }
}

#[test]
fn observation_and_unmanaged_control_never_touch_the_platform_pointer() {
    let seen = visible(7, 10, 20);
    let observing = resolve(Inputs {
        controlling: false,
        window: None,
        confirmed: seen,
        explained: false,
    });
    assert_eq!(observing.window, None);
    assert!(observing.overlay.is_some() && !observing.exclusive);
    // Without a platform owner, the always-drawn local pointer is the owner
    // while the host follows it; a divergent host position is still shown.
    let unmanaged = |explained| {
        resolve(Inputs {
            controlling: true,
            window: None,
            confirmed: seen,
            explained,
        })
    };
    assert_eq!(unmanaged(true).overlay, None);
    assert!(unmanaged(false).overlay.is_some());
    assert_eq!(unmanaged(false).window, None);
    let hidden = resolve(Inputs {
        controlling: true,
        window: None,
        confirmed: Confirmed::Hidden,
        explained: false,
    });
    assert_eq!((hidden.overlay, hidden.window), (None, None));
}

/// Deterministic model of the two owners: the presenter acknowledges an
/// overlay within its step; the platform acknowledges requests later.
struct Model {
    rendered: Rendered,
    generation: u64,
    platform: Option<(WindowCursor, u64)>,
    delay: u32,
}
impl Model {
    fn new() -> Self {
        let mut rendered = Rendered::new();
        rendered.window_attached();
        Self {
            rendered,
            generation: 0,
            platform: None,
            delay: 0,
        }
    }
    /// One turn: maybe acknowledge the platform, then take at most one step.
    fn turn(&mut self, target: &Target, lag: u32) -> Step {
        if let Some((_, generation)) = self.platform {
            if self.delay == 0 {
                self.rendered.window_applied(generation);
                self.platform = None;
            } else {
                self.delay -= 1;
            }
        }
        let step = self.rendered.next(target);
        match step {
            Step::Overlay(overlay) => self.rendered.overlay_applied(overlay),
            Step::Window(cursor) => {
                self.generation += 1;
                self.rendered.window_requested(cursor, self.generation);
                self.platform = Some((cursor, self.generation));
                self.delay = lag;
            }
            Step::Idle => {}
        }
        step
    }
}

#[test]
fn transitions_never_draw_both_owners_and_reach_every_target() {
    let converged = controlling(LocalPointer::Inside, visible(7, 10, 20), true);
    let moved_shape = controlling(LocalPointer::Inside, visible(8, 12, 22), true);
    let diverged = controlling(LocalPointer::Inside, visible(7, 300, 200), false);
    let diverged_moved = controlling(LocalPointer::Inside, visible(7, 310, 205), false);
    let outside = controlling(LocalPointer::Outside, visible(7, 10, 20), true);
    let hidden = controlling(LocalPointer::Inside, Confirmed::Hidden, true);
    let script = [
        converged,
        diverged,
        diverged_moved,
        converged,
        moved_shape,
        outside,
        hidden,
        diverged,
        hidden,
        converged,
    ];
    // Deterministic pseudo-random target churn and platform lag.
    let mut seed = 0x2545_f491_u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..200 {
        let mut model = Model::new();
        // An observation-era overlay is on screen when control takes over;
        // the very first step must end that pair.
        model.rendered.overlay_applied(Some(At {
            shape: 7,
            x: 1,
            y: 1,
        }));
        assert!(!model.rendered.exclusive());
        for (index, target) in script.iter().enumerate() {
            let lag = u32::try_from(next() % 4).unwrap();
            if next() % 3 == 0 {
                // Interrupted: one step toward another target first.
                model.turn(&script[(index + 3) % script.len()], lag);
                assert!(model.rendered.exclusive(), "round {round} target {index}");
            }
            let mut turns = 0;
            while model.rendered.overlay() != Overlay::from(target.overlay)
                || model.rendered.window() != target.window
                || model.platform.is_some()
            {
                model.turn(target, lag);
                assert!(
                    model.rendered.exclusive(),
                    "round {round} target {index} turn {turns}: {:?}",
                    model.rendered
                );
                turns += 1;
                assert!(
                    turns < 32,
                    "no convergence: {:?} -> {target:?}",
                    model.rendered
                );
            }
            assert_eq!(model.rendered.next(target), Step::Idle, "{index}");
        }
    }
}

#[test]
fn a_blank_local_pointer_is_acknowledged_before_the_overlay_appears() {
    let mut model = Model::new();
    model.turn(
        &controlling(LocalPointer::Inside, visible(7, 10, 20), true),
        0,
    );
    model.turn(
        &controlling(LocalPointer::Inside, visible(7, 10, 20), true),
        0,
    );
    assert_eq!(model.rendered.window(), Some(WindowCursor::Shape(7)));
    let diverged = controlling(LocalPointer::Inside, visible(7, 300, 200), false);
    assert_eq!(model.turn(&diverged, 5), Step::Window(WindowCursor::Blank));
    for _ in 0..5 {
        assert_eq!(model.turn(&diverged, 5), Step::Idle, "overlay before blank");
    }
    assert_eq!(
        model.turn(&diverged, 5),
        Step::Overlay(Some(At {
            shape: 7,
            x: 300,
            y: 200
        }))
    );
    // Back: the overlay is removed BEFORE the local pointer takes the shape.
    let back = controlling(LocalPointer::Inside, visible(7, 10, 20), true);
    assert_eq!(model.turn(&back, 0), Step::Overlay(None));
    assert_eq!(model.turn(&back, 0), Step::Window(WindowCursor::Shape(7)));
}

#[test]
fn a_detached_platform_owner_is_no_longer_managed() {
    let mut rendered = Rendered::new();
    rendered.window_attached();
    rendered.window_requested(WindowCursor::Blank, 1);
    rendered.window_detached();
    assert_eq!(rendered.window(), None);
    // A late acknowledgement cannot resurrect it.
    rendered.window_applied(1);
    assert_eq!(rendered.window(), None);
    let unmanaged = resolve(Inputs {
        controlling: true,
        window: None,
        confirmed: visible(7, 10, 20),
        explained: false,
    });
    assert_eq!(
        rendered.next(&unmanaged),
        Step::Overlay(Some(At {
            shape: 7,
            x: 10,
            y: 20
        }))
    );
}
