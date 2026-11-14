//! Remote control of a Windows host: the input owner's native sink. frd
//! injects with `SendInput` itself (`fr_winsys::input`), on the input owner's
//! own thread, right after the owner's final authority check; there is no
//! helper process. The run's Stop sharing window (`host_indicator`) stays up
//! for the whole run, and pressing it stops the run, which revokes input.
//!
//! The rules are the macOS owner's: a lease binds the one display whose
//! physical rectangle is exactly the granted bounds, and every positive
//! operation re-checks it. A press of a held key or button is refused, as is
//! a release or repeat of one not held; text waits until nothing is held.
//! Windows adds two checks before every positive operation: the input
//! desktop must be the person's ordinary desktop (not the lock screen or a
//! UAC prompt), and a key, button, wheel or text operation is refused while
//! the focused window belongs to a process of higher integrity (an elevated
//! program), which Windows would silently keep the input from. Releases are
//! never refused: cleanup must always be able to let go.
use fr_core::{
    input::{DesktopPoint, InputBounds, KeyTransition, PointerButton, ScrollUnit, windows_scan},
    input_submission::{
        Capabilities, Capability, InputSink, Operation, PlatformError, Submission,
        scroll::{LINE, LineScroll},
    },
};

/// A physical key as `SendInput` reproduces it (see `fr_winsys::input::Key`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Scan { code: u8, extended: bool },
    Pause,
    NumLock,
}

/// What one native call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    All,
    /// Nothing reached the input stream.
    Nothing,
    /// Part of a batch did.
    Partly,
}

/// What the sink asks of Windows; [`System`] is the real one.
pub trait Native {
    /// A fully prepared absolute move. Construction may query Windows; moving
    /// it into `move_to` may only submit the already prepared native operation.
    type PreparedMove;

    /// Every display's physical rectangle.
    fn displays(&mut self) -> Result<Vec<InputBounds>, PlatformError>;
    fn cursor(&mut self) -> Result<DesktopPoint, PlatformError>;
    /// The input desktop is the ordinary one.
    fn desktop_is_default(&mut self) -> bool;
    /// The focused window belongs to a process this one cannot send input to.
    fn foreground_outranks_us(&mut self) -> bool;
    fn key(&mut self, key: Key, down: bool) -> Sent;
    fn text(&mut self, scalar: char) -> Sent;
    fn prepare_move_to(&mut self, point: DesktopPoint) -> Result<Self::PreparedMove, PlatformError>;
    fn move_to(&mut self, prepared: Self::PreparedMove) -> Sent;
    fn button(&mut self, button: PointerButton, down: bool) -> Sent;
    /// Wheel deltas, 120 per notch: positive right, positive away from the user.
    fn scroll(&mut self, horizontal: i32, vertical: i32) -> Sent;
}

/// What this sink implements; the probe and every grant meet it.
pub fn implemented() -> Capabilities {
    Capabilities::default()
        .with(Capability::Keys)
        .with(Capability::Repeat)
        .with(Capability::Absolute)
        .with(Capability::Buttons)
        .with(Capability::LineScroll)
        .with(Capability::Text)
}

/// The key `SendInput` needs for HID usage `usage`.
fn native_key(usage: u16) -> Option<Key> {
    match usage {
        0x48 => Some(Key::Pause),
        0x53 => Some(Key::NumLock),
        _ => windows_scan::scan(usage).map(|(code, extended)| Key::Scan { code, extended }),
    }
}

const fn button_index(button: PointerButton) -> usize {
    button as usize - 1
}

/// One lease's input owner, bound to one display.
pub struct WindowsInput<N: Native> {
    native: N,
    bounds: InputBounds,
    /// Held keys by HID usage, and held buttons; a press that may have
    /// happened counts as held until a release is confirmed.
    keys: [bool; 256],
    buttons: [bool; 5],
    /// Where this owner last placed the pointer; presses and wheel turns act
    /// there. `None`: the real pointer, which must be on the display.
    position: Option<DesktopPoint>,
    prepared: Option<Operation>,
    prepared_move: Option<N::PreparedMove>,
}

impl<N: Native> WindowsInput<N> {
    /// Bind the display whose physical rectangle is exactly `bounds`, after
    /// checking every `required` capability is implemented. Nothing is sent.
    pub fn open(
        mut native: N,
        bounds: InputBounds,
        required: Capabilities,
    ) -> Result<Self, PlatformError> {
        if !implemented().contains_all(required) {
            return Err(PlatformError::Unsupported);
        }
        if !native.displays()?.contains(&bounds) {
            return Err(PlatformError::GeometryChanged);
        }
        Ok(Self {
            native,
            bounds,
            keys: [false; 256],
            buttons: [false; 5],
            position: None,
            prepared: None,
            prepared_move: None,
        })
    }

    fn holding(&self) -> bool {
        self.keys.contains(&true) || self.buttons.contains(&true)
    }

    /// Where a press or wheel turn acts; refused off the bound display.
    fn action_point(&mut self) -> Result<DesktopPoint, PlatformError> {
        let point = match self.position {
            Some(point) => point,
            None => self.native.cursor()?,
        };
        if self.bounds.contains(point) {
            Ok(point)
        } else {
            Err(PlatformError::GeometryChanged)
        }
    }

    fn check(&mut self, op: Operation) -> Result<(), PlatformError> {
        match op {
            Operation::Key { key, transition } => {
                native_key(key.usage()).ok_or(PlatformError::Unsupported)?;
                let held = self.keys[usize::from(key.usage())];
                match transition {
                    KeyTransition::Press if held => Err(PlatformError::Permission),
                    KeyTransition::Release | KeyTransition::Repeat if !held => {
                        Err(PlatformError::Unsupported)
                    }
                    _ => Ok(()),
                }
            }
            Operation::Absolute(point) if self.bounds.contains(point) => Ok(()),
            Operation::Absolute(_) => Err(PlatformError::GeometryChanged),
            Operation::Button { button, pressed } => {
                let held = self.buttons[button_index(button)];
                if pressed == held {
                    return Err(if pressed {
                        PlatformError::Permission
                    } else {
                        PlatformError::Unsupported
                    });
                }
                if pressed {
                    self.action_point()?;
                }
                Ok(())
            }
            Operation::Scroll {
                x,
                y,
                unit: ScrollUnit::Lines,
            } => {
                LineScroll::new(x, y).ok_or(PlatformError::Unsupported)?;
                if x == 0 && y == 0 {
                    return Err(PlatformError::Unsupported);
                }
                self.action_point().map(drop)
            }
            // Do not turn text into a shortcut, or release a held key.
            Operation::Text(_) if self.holding() => Err(PlatformError::Permission),
            Operation::Text(_) => Ok(()),
            Operation::Scroll { .. } | Operation::Relative { .. } | Operation::Wheel { .. } => {
                Err(PlatformError::Unsupported)
            }
        }
    }

    /// The native side of a positive operation: the person's desktop, the
    /// bound display, and a focused window input can reach.
    fn check_native(&mut self, op: Operation) -> Result<(), PlatformError> {
        if !self.native.desktop_is_default() {
            return Err(PlatformError::Unavailable);
        }
        if !self.native.displays()?.contains(&self.bounds) {
            return Err(PlatformError::GeometryChanged);
        }
        if !matches!(op, Operation::Absolute(_)) && self.native.foreground_outranks_us() {
            return Err(PlatformError::Permission);
        }
        Ok(())
    }

    fn send(&mut self, op: Operation) -> Submission {
        let sent = match op {
            Operation::Key { key, transition } => {
                let Some(native) = native_key(key.usage()) else {
                    return Submission::NotSubmitted(PlatformError::Unsupported);
                };
                let slot = usize::from(key.usage());
                let down = transition != KeyTransition::Release;
                let was = self.keys[slot];
                // Held before the call, so an uncertain press is released.
                self.keys[slot] |= down;
                let sent = self.native.key(native, down);
                match (sent, down) {
                    (Sent::All, false) => self.keys[slot] = false,
                    (Sent::Nothing, true) => self.keys[slot] = was,
                    _ => {}
                }
                sent
            }
            Operation::Absolute(point) => {
                let Some(prepared) = self.prepared_move.take() else {
                    return Submission::NotSubmitted(PlatformError::Unsupported);
                };
                let sent = self.native.move_to(prepared);
                match sent {
                    Sent::All => self.position = Some(point),
                    Sent::Partly => self.position = None,
                    Sent::Nothing => {}
                }
                sent
            }
            Operation::Button { button, pressed } => {
                let slot = button_index(button);
                let was = self.buttons[slot];
                self.buttons[slot] |= pressed;
                let sent = self.native.button(button, pressed);
                match (sent, pressed) {
                    (Sent::All, false) => self.buttons[slot] = false,
                    (Sent::Nothing, true) => self.buttons[slot] = was,
                    _ => {}
                }
                sent
            }
            Operation::Scroll {
                x,
                y,
                unit: ScrollUnit::Lines,
            } => {
                // Protocol: positive x right, positive y down (toward the
                // user). Windows: positive horizontal right, positive
                // vertical away from the user. LineScroll bounds both.
                let (Some(horizontal), Some(vertical)) =
                    ((x / LINE).checked_mul(120), (y / LINE).checked_mul(-120))
                else {
                    return Submission::NotSubmitted(PlatformError::Unsupported);
                };
                self.native.scroll(horizontal, vertical)
            }
            Operation::Text(scalar) => self.native.text(scalar),
            _ => return Submission::NotSubmitted(PlatformError::Unsupported),
        };
        match sent {
            Sent::All => Submission::Submitted,
            Sent::Nothing => Submission::NotSubmitted(PlatformError::Unavailable),
            Sent::Partly => Submission::Unknown,
        }
    }

    /// Release every key and button this owner holds. True when nothing is
    /// left held; false means a release was not confirmed.
    pub fn native_cleanup(&mut self) -> bool {
        self.prepared = None;
        self.prepared_move = None;
        for usage in 0..self.keys.len() {
            if self.keys[usage]
                && let Some(key) = u16::try_from(usage).ok().and_then(native_key)
                && self.native.key(key, false) == Sent::All
            {
                self.keys[usage] = false;
            }
        }
        for (slot, button) in [
            PointerButton::Primary,
            PointerButton::Secondary,
            PointerButton::Middle,
            PointerButton::Back,
            PointerButton::Forward,
        ]
        .into_iter()
        .enumerate()
        {
            if self.buttons[slot] && self.native.button(button, false) == Sent::All {
                self.buttons[slot] = false;
            }
        }
        !self.holding()
    }
}

impl<N: Native> InputSink for WindowsInput<N> {
    fn prepare(&mut self, operation: Operation) -> Result<(), PlatformError> {
        self.prepared = None;
        self.prepared_move = None;
        self.check(operation)?;
        if !operation.is_release() {
            self.check_native(operation)?;
        }
        if let Operation::Absolute(point) = operation {
            // Display enumeration and normalization can stall. Finish them
            // before InputSession samples the original authority deadline.
            self.prepared_move = Some(self.native.prepare_move_to(point)?);
        }
        self.prepared = Some(operation);
        Ok(())
    }
    /// Called on this thread directly after the owner's final check (or for
    /// a cleanup release): only a prepared operation is sent.
    fn submit(&mut self, operation: Operation) -> Submission {
        if self.prepared.take() != Some(operation) {
            self.prepared_move = None;
            return Submission::NotSubmitted(PlatformError::Unsupported);
        }
        self.send(operation)
    }
    fn cancel_prepared(&mut self) {
        self.prepared = None;
        self.prepared_move = None;
    }
}

impl<N: Native> Drop for WindowsInput<N> {
    fn drop(&mut self) {
        let _ = self.native_cleanup();
    }
}

/// The real Windows calls.
#[cfg(windows)]
pub struct System;

#[cfg(windows)]
impl System {
    fn sent(result: Result<(), fr_winsys::input::NotInserted>) -> Sent {
        match result {
            Ok(()) => Sent::All,
            Err(failure) if failure.inserted == 0 => Sent::Nothing,
            Err(_) => Sent::Partly,
        }
    }
}

#[cfg(windows)]
impl Native for System {
    type PreparedMove = fr_winsys::input::PreparedMove;

    fn displays(&mut self) -> Result<Vec<InputBounds>, PlatformError> {
        let displays = fr_winsys::input::monitors().map_err(|_| PlatformError::Unavailable)?;
        Ok(displays
            .into_iter()
            .filter_map(|d| {
                InputBounds::new(
                    DesktopPoint {
                        x: d.left,
                        y: d.top,
                    },
                    d.width,
                    d.height,
                )
            })
            .collect())
    }
    fn cursor(&mut self) -> Result<DesktopPoint, PlatformError> {
        fr_winsys::input::cursor()
            .map(|(x, y)| DesktopPoint { x, y })
            .map_err(|_| PlatformError::Unavailable)
    }
    fn desktop_is_default(&mut self) -> bool {
        fr_winsys::input::input_desktop_is_default()
    }
    fn foreground_outranks_us(&mut self) -> bool {
        fr_winsys::input::foreground_outranks_us()
    }
    fn key(&mut self, key: Key, down: bool) -> Sent {
        use fr_winsys::input::Key as K;
        let key = match key {
            Key::Scan { code, extended } => K::Scan { code, extended },
            Key::Pause => K::Pause,
            Key::NumLock => K::NumLock,
        };
        Self::sent(fr_winsys::input::key(key, down))
    }
    fn text(&mut self, scalar: char) -> Sent {
        Self::sent(fr_winsys::input::text(scalar))
    }
    fn prepare_move_to(&mut self, point: DesktopPoint) -> Result<Self::PreparedMove, PlatformError> {
        fr_winsys::input::PreparedMove::new(point.x, point.y).map_err(|_| PlatformError::Unavailable)
    }
    fn move_to(&mut self, prepared: Self::PreparedMove) -> Sent {
        Self::sent(prepared.submit())
    }
    fn button(&mut self, button: PointerButton, down: bool) -> Sent {
        use fr_winsys::input::Button as B;
        let button = match button {
            PointerButton::Primary => B::Left,
            PointerButton::Secondary => B::Right,
            PointerButton::Middle => B::Middle,
            PointerButton::Back => B::Back,
            PointerButton::Forward => B::Forward,
        };
        Self::sent(fr_winsys::input::button(button, down))
    }
    fn scroll(&mut self, horizontal: i32, vertical: i32) -> Sent {
        Self::sent(fr_winsys::input::scroll(horizontal, vertical))
    }
}

/// `frd run --control`'s startup probe: what a grant may ask for, when this
/// process can see at least one display.
#[cfg(windows)]
pub fn probe() -> Result<Capabilities, PlatformError> {
    if System.displays()?.is_empty() {
        return Err(PlatformError::Unavailable);
    }
    Ok(implemented())
}

/// The native owner's factory for one lease (runs on the input thread).
#[cfg(windows)]
pub fn open(
    bounds: InputBounds,
    required: Capabilities,
) -> Result<WindowsInput<System>, PlatformError> {
    WindowsInput::open(System, bounds, required)
}

#[cfg(test)]
mod tests;
