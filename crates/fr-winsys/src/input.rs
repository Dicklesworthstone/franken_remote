//! Synthetic keyboard and pointer input for a Windows host (`SendInput`), and
//! the reads that say whether that input can reach what the person sees: the
//! displays' physical rectangles, the pointer, whether the input desktop is
//! the ordinary one (not the lock screen or a UAC prompt), and whether the
//! focused window belongs to a process of higher integrity, which Windows
//! would silently keep the input from (UIPI).
//!
//! Every coordinate here is a physical pixel of the virtual desktop: each
//! call runs per-monitor DPI aware, whatever its thread was.
use std::{
    io, mem,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
};
use windows::Win32::{
    Foundation::{HANDLE, LPARAM, POINT, RECT},
    Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR},
    Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
        TOKEN_QUERY, TokenIntegrityLevel,
    },
    System::{
        StationsAndDesktops::{
            CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetUserObjectInformationW,
            OpenInputDesktop, UOI_NAME,
        },
        Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            SetThreadDpiAwarenessContext,
        },
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
            KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
            MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
            MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
            MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
            MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_NUMLOCK,
            VK_PAUSE,
        },
        WindowsAndMessaging::{
            GetCursorPos, GetForegroundWindow, GetWindowThreadProcessId, XBUTTON1, XBUTTON2,
        },
    },
};
use windows::core::BOOL;

/// One display's rectangle in physical virtual-desktop pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}
impl Rect {
    fn from_win(r: &RECT) -> Option<Self> {
        Some(Self {
            left: r.left,
            top: r.top,
            width: u32::try_from(r.right.checked_sub(r.left)?).ok()?,
            height: u32::try_from(r.bottom.checked_sub(r.top)?).ok()?,
        })
    }
    fn right(self) -> i64 {
        i64::from(self.left) + i64::from(self.width)
    }
    fn bottom(self) -> i64 {
        i64::from(self.top) + i64::from(self.height)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Restores the thread's DPI awareness on every path out.
struct PerMonitor(DPI_AWARENESS_CONTEXT);
impl PerMonitor {
    fn enter() -> Self {
        // SAFETY: a per-thread setting, restored by Drop.
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}
impl Drop for PerMonitor {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the context this thread had before `enter`.
            let _ = unsafe { SetThreadDpiAwarenessContext(self.0) };
        }
    }
}

/// Every display attached to the desktop, in physical pixels.
pub fn monitors() -> io::Result<Vec<Rect>> {
    unsafe extern "system" fn visit(_: HMONITOR, _: HDC, rect: *mut RECT, found: LPARAM) -> BOOL {
        // SAFETY: Windows passes the monitor's rectangle; `found` is the
        // vector `monitors` lent for the duration of the enumeration.
        unsafe {
            let found = &mut *(found.0 as *mut Vec<Rect>);
            if let Some(rect) = rect.as_ref().and_then(Rect::from_win) {
                found.push(rect);
            }
        }
        BOOL(1)
    }
    let _aware = PerMonitor::enter();
    let mut found: Vec<Rect> = Vec::new();
    // SAFETY: the callback only writes into `found`, which outlives the call.
    let ok =
        unsafe { EnumDisplayMonitors(None, None, Some(visit), LPARAM((&raw mut found) as isize)) };
    if ok.as_bool() {
        Ok(found)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The smallest rectangle holding every display (the virtual desktop).
fn virtual_desktop(displays: &[Rect]) -> Option<Rect> {
    let left = displays.iter().map(|r| r.left).min()?;
    let top = displays.iter().map(|r| r.top).min()?;
    let right = displays.iter().map(|r| r.right()).max()?;
    let bottom = displays.iter().map(|r| r.bottom()).max()?;
    Some(Rect {
        left,
        top,
        width: u32::try_from(right - i64::from(left)).ok()?,
        height: u32::try_from(bottom - i64::from(top)).ok()?,
    })
}

/// `SendInput`'s 0..=65535 scale for pixel `p` of a span starting at
/// `start` and `size` pixels wide: Windows places normalized `n` at pixel
/// `start + floor(n * size / 65536)`, so the smallest `n` reaching `p` is
/// the ceiling of the inverse.
pub fn normalized(p: i32, start: i32, size: u32) -> Option<i32> {
    let offset = i64::from(p) - i64::from(start);
    let size = i64::from(size);
    if size == 0 || offset < 0 || offset >= size {
        return None;
    }
    i32::try_from((offset * 65536 + size - 1) / size).ok()
}

/// `SendInput` did not insert a whole batch: none of it (`inserted == 0`,
/// so nothing happened) or only its first `inserted` events.
#[derive(Debug)]
pub struct NotInserted {
    pub inserted: usize,
    pub error: io::Error,
}
impl From<io::Error> for NotInserted {
    fn from(error: io::Error) -> Self {
        Self { inserted: 0, error }
    }
}
impl From<io::ErrorKind> for NotInserted {
    fn from(kind: io::ErrorKind) -> Self {
        io::Error::from(kind).into()
    }
}

fn send(inputs: &[INPUT]) -> Result<(), NotInserted> {
    let size = i32::try_from(mem::size_of::<INPUT>()).map_err(|_| io::ErrorKind::InvalidInput)?;
    // SAFETY: a slice of fully initialized INPUT records and their size.
    let sent = unsafe { SendInput(inputs, size) };
    let inserted = usize::try_from(sent).unwrap_or(0);
    if inserted == inputs.len() {
        Ok(())
    } else {
        Err(NotInserted {
            inserted,
            error: io::Error::last_os_error(),
        })
    }
}

fn keyboard(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// A physical key, as `SendInput` can reproduce it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// By set-1 scan code (`extended`: E0-prefixed), as the keyboard sends
    /// it: the active layout decides what it means.
    Scan {
        code: u8,
        extended: bool,
    },
    /// Pause and Num Lock do not round-trip through `SendInput` by scan code
    /// (Pause is an E1 sequence; Num Lock's code is Pause's), so they go by
    /// virtual key, Num Lock with its extended flag as the keyboard sets it.
    Pause,
    NumLock,
}

/// One key transition.
pub fn key(key: Key, down: bool) -> Result<(), NotInserted> {
    let up = if down {
        KEYBD_EVENT_FLAGS::default()
    } else {
        KEYEVENTF_KEYUP
    };
    let input = match key {
        Key::Scan { code, extended } => {
            let extended = if extended {
                KEYEVENTF_EXTENDEDKEY
            } else {
                KEYBD_EVENT_FLAGS::default()
            };
            keyboard(0, u16::from(code), KEYEVENTF_SCANCODE | extended | up)
        }
        Key::Pause => keyboard(VK_PAUSE.0, 0, up),
        Key::NumLock => keyboard(VK_NUMLOCK.0, 0, KEYEVENTF_EXTENDEDKEY | up),
    };
    send(&[input])
}

/// Type one Unicode scalar (one or two UTF-16 units, each pressed and
/// released) without a physical key: what the layout cannot produce still
/// arrives as that character.
pub fn text(scalar: char) -> Result<(), NotInserted> {
    let mut units = [0_u16; 2];
    let mut inputs = Vec::with_capacity(4);
    for &unit in scalar.encode_utf16(&mut units).iter() {
        inputs.push(keyboard(0, unit, KEYEVENTF_UNICODE));
        inputs.push(keyboard(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
    }
    send(&inputs)
}

/// Move the pointer to physical pixel (`x`, `y`) of the virtual desktop.
pub fn move_to(x: i32, y: i32) -> Result<(), NotInserted> {
    let _aware = PerMonitor::enter();
    let desktop = virtual_desktop(&monitors()?).ok_or(io::ErrorKind::NotFound)?;
    let (Some(nx), Some(ny)) = (
        normalized(x, desktop.left, desktop.width),
        normalized(y, desktop.top, desktop.height),
    ) else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    send(&[mouse(
        nx,
        ny,
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )])
}

/// Press or release one button where the pointer is.
pub fn button(button: Button, down: bool) -> Result<(), NotInserted> {
    let (flags, data) = match (button, down) {
        (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (Button::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
        (Button::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
        (Button::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
        (Button::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
    };
    send(&[mouse(0, 0, u32::from(data), flags)])
}

/// Turn the wheel where the pointer is, 120 per notch: `horizontal`
/// (positive: right) then `vertical` (positive: away from the user). A zero
/// axis sends nothing.
pub fn scroll(horizontal: i32, vertical: i32) -> Result<(), NotInserted> {
    // mouseData carries the signed delta in its bits.
    let inputs: Vec<INPUT> = [
        (horizontal, MOUSEEVENTF_HWHEEL),
        (vertical, MOUSEEVENTF_WHEEL),
    ]
    .into_iter()
    .filter(|&(delta, _)| delta != 0)
    .map(|(delta, flags)| mouse(0, 0, delta.cast_unsigned(), flags))
    .collect();
    if inputs.is_empty() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    send(&inputs)
}

/// Where the pointer is, in physical virtual-desktop pixels.
pub fn cursor() -> io::Result<(i32, i32)> {
    let _aware = PerMonitor::enter();
    let mut point = POINT::default();
    // SAFETY: an out pointer to a local.
    unsafe { GetCursorPos(&raw mut point) }?;
    Ok((point.x, point.y))
}

/// The desktop receiving input is the ordinary one ("Default"), not the
/// lock screen or a UAC prompt (the secure desktop refuses to open at all).
pub fn input_desktop_is_default() -> bool {
    // SAFETY: opening the input desktop for reading its name; closed below.
    let Ok(desktop) =
        (unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) })
    else {
        return false;
    };
    let mut name = [0_u16; 64];
    let mut needed = 0_u32;
    // SAFETY: a writable buffer of the stated byte length for the name.
    let read = unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            u32::try_from(mem::size_of_val(&name)).unwrap_or(0),
            Some(&raw mut needed),
        )
    };
    // SAFETY: the desktop handle opened above, closed once.
    let _ = unsafe { CloseDesktop(desktop) };
    let end = name.iter().position(|&u| u == 0).unwrap_or(name.len());
    read.is_ok() && String::from_utf16_lossy(&name[..end]).eq_ignore_ascii_case("Default")
}

/// A process's mandatory integrity level (0x2000 medium, 0x3000 high, ...).
fn integrity(process: HANDLE) -> io::Result<u32> {
    let mut raw = HANDLE::default();
    // SAFETY: a process handle with query rights and an out pointer.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut raw) }?;
    // SAFETY: the token handle just opened, owned only here.
    let token = unsafe { OwnedHandle::from_raw_handle(raw.0) };
    let token = HANDLE(token.as_raw_handle());
    let mut needed = 0_u32;
    // SAFETY: a size query; it fails with ERROR_INSUFFICIENT_BUFFER by design.
    let _ = unsafe { GetTokenInformation(token, TokenIntegrityLevel, None, 0, &raw mut needed) };
    // u64 words keep the SID pointer inside the buffer aligned.
    let mut buffer = vec![0_u64; (needed as usize).div_ceil(8).max(1)];
    let length = u32::try_from(buffer.len() * 8).map_err(|_| io::ErrorKind::InvalidData)?;
    // SAFETY: a writable buffer of `length` bytes and an out pointer.
    unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buffer.as_mut_ptr().cast()),
            length,
            &raw mut needed,
        )
    }?;
    // SAFETY: the call filled the buffer with a TOKEN_MANDATORY_LABEL whose
    // SID lies inside it; its last sub-authority is the level.
    unsafe {
        let sid = (*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid;
        let count = *GetSidSubAuthorityCount(sid);
        let last = u32::from(count.checked_sub(1).ok_or(io::ErrorKind::InvalidData)?);
        Ok(*GetSidSubAuthority(sid, last))
    }
}

/// The focused window belongs to a process this one cannot send input to:
/// a higher integrity level (an elevated program), or one whose level
/// cannot be read. No focused window: false.
pub fn foreground_outranks_us() -> bool {
    // SAFETY: queries with no pointers.
    let window = unsafe { GetForegroundWindow() };
    if window.is_invalid() {
        return false;
    }
    let mut pid = 0_u32;
    // SAFETY: an out pointer to a local.
    unsafe { GetWindowThreadProcessId(window, Some(&raw mut pid)) };
    if pid == 0 {
        return true;
    }
    // SAFETY: the current-process pseudo handle needs no closing.
    let Ok(ours) = integrity(unsafe { GetCurrentProcess() }) else {
        return true;
    };
    // SAFETY: opening the process for a limited query; owned below.
    let Ok(raw) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return true;
    };
    // SAFETY: the handle just opened, owned only here.
    let process = unsafe { OwnedHandle::from_raw_handle(raw.0) };
    integrity(HANDLE(process.as_raw_handle())).map_or(true, |theirs| theirs > ours)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_coordinates_land_on_the_requested_pixel() {
        for size in [1_u32, 2, 1080, 1504, 2256, 3840, 7680] {
            for p in [0, 1.min(size - 1), size / 2, size - 1] {
                let p = i32::try_from(p).unwrap();
                let n = normalized(p, 0, size).unwrap();
                assert!((0..=65535).contains(&n), "{size} {p} {n}");
                // Windows' mapping: floor(n * size / 65536).
                let landed = i64::from(n) * i64::from(size) / 65536;
                assert_eq!(landed, i64::from(p), "{size} {p} {n}");
            }
        }
        // Offsets from a negative origin, and pixels outside the span.
        assert_eq!(normalized(-1920, -1920, 3840), Some(0));
        assert_eq!(normalized(-1921, -1920, 3840), None);
        assert_eq!(normalized(1920, -1920, 3840), None);
        assert_eq!(normalized(0, 0, 0), None);
    }

    #[test]
    fn the_virtual_desktop_spans_every_display() {
        let left = Rect {
            left: -1920,
            top: 200,
            width: 1920,
            height: 1080,
        };
        let main = Rect {
            left: 0,
            top: 0,
            width: 2256,
            height: 1504,
        };
        assert_eq!(
            virtual_desktop(&[main, left]),
            Some(Rect {
                left: -1920,
                top: 0,
                width: 4176,
                height: 1504
            })
        );
        assert_eq!(virtual_desktop(&[]), None);
    }

    /// Reads only; run in the logged-on session.
    #[test]
    #[ignore = "reads the interactive desktop"]
    fn the_logged_on_desktop_reads_as_default_with_the_pointer_on_a_display() {
        let displays = monitors().unwrap();
        assert_ne!(displays, [] as [Rect; 0]);
        assert!(input_desktop_is_default());
        let (x, y) = cursor().unwrap();
        assert!(
            displays.iter().any(|d| i64::from(x) >= i64::from(d.left)
                && i64::from(x) < d.right()
                && i64::from(y) >= i64::from(d.top)
                && i64::from(y) < d.bottom()),
            "{displays:?}"
        );
        eprintln!(
            "displays {displays:?}; foreground outranks us: {}",
            foreground_outranks_us()
        );
    }

    /// MOVES THE REAL POINTER: only with `FR_TEST_MOVE_POINTER=1`, on a machine
    /// whose owner agreed. Each corner of each display, then back.
    #[test]
    #[ignore = "moves the pointer"]
    fn absolute_moves_land_on_the_exact_pixel() {
        if std::env::var_os("FR_TEST_MOVE_POINTER").is_none() {
            eprintln!("SKIPPED: set FR_TEST_MOVE_POINTER=1 to move the pointer");
            return;
        }
        let start = cursor().unwrap();
        for d in monitors().unwrap() {
            let right = d.left + i32::try_from(d.width).unwrap() - 1;
            let bottom = d.top + i32::try_from(d.height).unwrap() - 1;
            for (x, y) in [
                (d.left, d.top),
                (right, d.top),
                (d.left, bottom),
                (right, bottom),
            ] {
                move_to(x, y).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(30));
                assert_eq!(cursor().unwrap(), (x, y));
            }
        }
        move_to(start.0, start.1).unwrap();
    }
}
