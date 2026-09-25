use super::*;
use fr_core::limits::ProtocolLimits;

fn frame(width: u32, height: u32, bgra: [u8; 4]) -> BgraFrame {
    let bytes = bgra.repeat((width * height) as usize);
    BgraFrame::new(width, height, bytes, &ProtocolLimits::ABSOLUTE).unwrap()
}
fn pixel(frame: &BgraFrame, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * frame.width + x) * 4) as usize;
    frame.bytes[at..at + 4].try_into().unwrap()
}
const GREY: [u8; 4] = [0x40, 0x40, 0x40, 0xFF];
// RGBA opaque magenta and a half-transparent red.
const MAGENTA: [u8; 4] = [0xFF, 0x00, 0xFF, 0xFF];

fn shape(x: i32, y: i32, rgba: &[u8], size: u16, hotspot: u16) -> Update<'_> {
    Update::Shape {
        x,
        y,
        width: size,
        height: size,
        hotspot_x: hotspot,
        hotspot_y: hotspot,
        rgba,
    }
}

#[test]
fn opaque_cursor_is_drawn_at_the_hotspot_and_moves_without_residue() {
    let mut f = frame(32, 32, GREY);
    let rgba = MAGENTA.repeat(4 * 4);
    let mut overlay = CursorOverlay::new();
    overlay.apply(&shape(10, 12, &rgba, 4, 1)).unwrap();
    overlay
        .composite_fresh(&mut f, Placement::Identity)
        .unwrap();
    // Top-left is the hotspot minus (1, 1); stored as BGRA.
    assert_eq!(pixel(&f, 9, 11), [0xFF, 0x00, 0xFF, 0xFF]);
    assert_eq!(pixel(&f, 12, 14), [0xFF, 0x00, 0xFF, 0xFF]);
    assert_eq!(pixel(&f, 13, 15), GREY);
    assert_eq!(pixel(&f, 8, 11), GREY);
    overlay.apply(&Update::Move { x: 20, y: 20 }).unwrap();
    overlay.recomposite(&mut f, Placement::Identity).unwrap();
    assert_eq!(pixel(&f, 9, 11), GREY, "old location restored exactly");
    assert_eq!(pixel(&f, 19, 19), [0xFF, 0x00, 0xFF, 0xFF]);
    overlay.apply(&Update::Hidden).unwrap();
    overlay.recomposite(&mut f, Placement::Identity).unwrap();
    assert_eq!(
        f.bytes,
        frame(32, 32, GREY).bytes,
        "hidden leaves no overlay"
    );
    assert!(!overlay.is_shown());
    assert!(!format!("{overlay:?}").contains("255"));
}

#[test]
fn straight_alpha_blends_and_is_clipped_to_the_picture() {
    let mut f = frame(16, 16, [0, 0, 0, 0xFF]);
    // Half-transparent pure red over black: ~128 in the red channel only.
    let rgba = [0xFF, 0x00, 0x00, 0x80].repeat(4 * 4);
    let mut overlay = CursorOverlay::new();
    // Hotspot at the bottom-right corner: most of the cursor falls outside.
    overlay.apply(&shape(15, 15, &rgba, 4, 0)).unwrap();
    overlay
        .composite_fresh(&mut f, Placement::Identity)
        .unwrap();
    assert_eq!(pixel(&f, 15, 15), [0x00, 0x00, 0x80, 0xFF]);
    assert_eq!(pixel(&f, 14, 15), [0, 0, 0, 0xFF]);
    // Entirely outside a fresh picture: nothing drawn, nothing saved, no panic
    // (and nothing restored from the previous picture's saved pixels).
    for (x, y) in [(-10, 3), (3, 40), (i32::MIN, i32::MAX)] {
        let mut g = frame(16, 16, GREY);
        overlay.apply(&Update::Move { x, y }).unwrap();
        overlay
            .composite_fresh(&mut g, Placement::Identity)
            .unwrap();
        assert_eq!(g.bytes, frame(16, 16, GREY).bytes);
        overlay.recomposite(&mut g, Placement::Identity).unwrap();
        assert_eq!(g.bytes, frame(16, 16, GREY).bytes);
    }
}

#[test]
fn fitted_placement_maps_the_hotspot_and_never_draws_into_letterbox_bars() {
    // A 64x32 picture fitted into a 32x32 window occupies y 8..24.
    let fit = Fit {
        x: 0,
        y: 8,
        width: 32,
        height: 16,
    };
    let placement = Placement::Fitted {
        fit,
        source_width: 64,
        source_height: 32,
    };
    let mut f = frame(32, 32, GREY);
    let rgba = MAGENTA.repeat(2 * 2);
    let mut overlay = CursorOverlay::new();
    overlay.apply(&shape(40, 10, &rgba, 2, 0)).unwrap();
    overlay.composite_fresh(&mut f, placement).unwrap();
    // (40, 10) in the source maps to (20, 13) in the window.
    assert_eq!(pixel(&f, 20, 13), [0xFF, 0x00, 0xFF, 0xFF]);
    assert_eq!(pixel(&f, 21, 14), [0xFF, 0x00, 0xFF, 0xFF]);
    // A hotspot just above the picture maps to window row 7 (inside the bar):
    // only its second row, at the picture's first row, is drawn.
    overlay.apply(&Update::Move { x: 0, y: -2 }).unwrap();
    overlay.recomposite(&mut f, placement).unwrap();
    assert_eq!(pixel(&f, 0, 7), GREY, "no pixels in the letterbox bar");
    assert_eq!(pixel(&f, 0, 8), [0xFF, 0x00, 0xFF, 0xFF]);
    assert_eq!(pixel(&f, 20, 13), GREY);
}

#[test]
fn hostile_updates_are_refused_before_allocation() {
    let mut overlay = CursorOverlay::new();
    assert_eq!(
        overlay.apply(&Update::Move { x: 1, y: 1 }),
        Err(NativeError::InvalidConfiguration),
        "a position without any shape is refused"
    );
    let rgba = [0_u8; 16];
    assert_eq!(
        overlay.apply(&Update::Shape {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
            hotspot_x: 2,
            hotspot_y: 0,
            rgba: &rgba,
        }),
        Err(NativeError::InvalidConfiguration)
    );
    assert_eq!(overlay.retained_bytes(), 0);
}
