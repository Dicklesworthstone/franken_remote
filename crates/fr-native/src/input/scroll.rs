//! Discrete X11 wheel effects. One prepared transition is one `XTest` request.
//! Core policy, not this module, expands an action or checks its original ticket.
use super::{PlatformError, X11Pointer, XGetPointerMapping, XSync, XTestFakeButtonEvent, c_int};
use fr_core::input_submission::scroll::WheelDirection;

#[derive(Default)]
pub(super) struct WheelState {
    held: Option<(WheelDirection, u8)>,
    pub(super) prepared: Option<(WheelDirection, bool, u8)>,
}
fn logical(direction: WheelDirection) -> u8 {
    match direction {
        WheelDirection::Up => 4,
        WheelDirection::Down => 5,
        WheelDirection::Left => 6,
        WheelDirection::Right => 7,
    }
}
impl X11Pointer {
    fn wheel_mapping(&self) -> Result<[u8; 4], PlatformError> {
        let mut map = [0u8; 256];
        // SAFETY: this thread owns the live connection and full-size output.
        let count = unsafe { XGetPointerMapping(self.display.as_ptr(), map.as_mut_ptr(), 256) };
        let count = usize::try_from(count)
            .ok()
            .filter(|n| (1..=256).contains(n))
            .ok_or(PlatformError::Unavailable)?;
        let mut wheel = [0; 4];
        for (physical, &value) in map[..count].iter().enumerate() {
            if (4..=7).contains(&value) {
                let slot = &mut wheel[usize::from(value - 4)];
                if *slot != 0 {
                    return Err(PlatformError::Unsupported);
                }
                *slot = u8::try_from(physical + 1).map_err(|_| PlatformError::Unsupported)?;
            }
        }
        if wheel.contains(&0) {
            return Err(PlatformError::Unsupported);
        }
        Ok(wheel)
    }
    pub(super) fn wheel_mapping_available(&self) -> bool {
        self.wheel_mapping().is_ok()
    }
    pub(super) fn prepare_wheel(
        &mut self,
        direction: WheelDirection,
        pressed: bool,
    ) -> Result<(), PlatformError> {
        let code = if pressed {
            if !self.line_scroll {
                return Err(PlatformError::Unsupported);
            }
            if self.wheel.held.is_some() {
                return Err(PlatformError::Permission);
            }
            if self.geometry()? != self.dimensions {
                return Err(PlatformError::GeometryChanged);
            }
            let code = self.wheel_mapping()?[usize::from(logical(direction) - 4)];
            if self.buttons.contains(&Some(code)) {
                return Err(PlatformError::Permission);
            }
            // Core X11 exposes held bits only for logical buttons 1..=5. Do not
            // claim equivalent physical/synthetic attribution for horizontal wheels.
            let (_, mask) = self.query_pointer()?;
            if logical(direction) <= 5 && mask & (1u32 << (7 + logical(direction))) != 0 {
                return Err(PlatformError::Permission);
            }
            code
        } else {
            let (held, code) = self.wheel.held.ok_or(PlatformError::Unsupported)?;
            if held != direction {
                return Err(PlatformError::Unsupported);
            }
            // Release the recorded physical code, not a fresh mapping or geometry.
            code
        };
        self.wheel.prepared = Some((direction, pressed, code));
        Ok(())
    }
    pub(super) fn submit_wheel(&mut self, direction: WheelDirection, pressed: bool) -> c_int {
        let Some((expected, down, code)) = self.wheel.prepared.take() else {
            return 0;
        };
        if (direction, pressed) != (expected, down) {
            return 0;
        }
        if pressed {
            // Retain uncertain native ownership BEFORE entering foreign code.
            self.wheel.held = Some((direction, code));
        }
        // SAFETY: outer submit retains the XTest-cache guard and live connection.
        // One scalar-only event, delay=0, with no retained Rust pointers.
        let result = unsafe {
            XTestFakeButtonEvent(
                self.display.as_ptr(),
                u32::from(code),
                c_int::from(pressed),
                0,
            )
        };
        if result != 0 && !pressed {
            self.wheel.held = None;
        }
        result
    }
    pub(super) fn cleanup_wheel(&mut self) -> bool {
        let Some((direction, code)) = self.wheel.held else {
            return true;
        };
        // SAFETY: cleanup_native retains cache exclusivity; release ONLY an
        // owned/possibly owned press, never a new scroll or a mapped substitute.
        let accepted = unsafe {
            let accepted = XTestFakeButtonEvent(self.display.as_ptr(), u32::from(code), 0, 0);
            XSync(self.display.as_ptr(), 0);
            accepted
        };
        if accepted == 0
            || (logical(direction) <= 5
                && !self
                    .query_pointer()
                    .is_ok_and(|(_, mask)| mask & (1u32 << (7 + logical(direction))) == 0))
        {
            return false;
        }
        self.wheel.held = None;
        true
    }
}
