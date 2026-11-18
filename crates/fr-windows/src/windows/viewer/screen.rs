//! Full screen and Windows-shortcut passthrough for the one viewer window.
//!
//! Full screen is a borderless popup over the window's monitor, so the
//! client area, and with it the picture the host is asked for, is exactly the
//! monitor's own pixel size: the host scales once and nothing is resampled
//! here. Ctrl+Alt+Enter toggles it; the resize reopens the view at the new
//! size like any other resize.
//!
//! Windows acts on some keys before any window sees them: the Windows key,
//! Alt+Tab, Alt+Esc, Ctrl+Esc, Alt+Space and so on. While this window is in
//! front and its session controls the host, a low-level keyboard hook takes
//! every physical key away from Windows and posts it to this window as an
//! ordinary key message, which the existing key path sends to the host (the
//! Windows key arrives on a Mac as Command). Ctrl+Alt+Del and Win+L stay
//! with Windows: no hook can take them. Ctrl+Alt+Enter is never sent.
use std::cell::{Cell, RefCell};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::KeyboardAndMouse::{
            VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_RCONTROL, VK_RETURN, VK_RMENU, VK_RSHIFT,
        },
        WindowsAndMessaging::{
            CallNextHookEx, GWL_STYLE, GetForegroundWindow, GetMenu, GetWindowLongPtrW,
            GetWindowRect, HC_ACTION, HHOOK, HMENU, HWND_TOP, KBDLLHOOKSTRUCT, LLKHF_EXTENDED,
            LLKHF_UP, PostMessageW, SWP_FRAMECHANGED, SWP_NOOWNERZORDER, SWP_NOZORDER, SetMenu,
            SetWindowLongPtrW, SetWindowPos, SetWindowsHookExW, UnhookWindowsHookEx,
            WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_KEYUP, WS_MAXIMIZE, WS_OVERLAPPEDWINDOW,
            WS_POPUP,
        },
    },
};

/// Posted by the hook (or the key path) to toggle full screen outside it.
pub(super) const TOGGLE_FULLSCREEN: u32 = WM_APP + 0x46;

struct Saved {
    /// Only the frame bits full screen removed. Restoring the whole earlier
    /// style would also restore an earlier hidden state: a view that starts
    /// full screen enters it before the window is first shown.
    frame: u32,
    rect: RECT,
    menu: HMENU,
}

/// `GWL_STYLE` is a 32-bit `DWORD` carried in a `LONG_PTR`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the window style is a 32-bit DWORD in a LONG_PTR"
)]
fn style_of(hwnd: HWND) -> u32 {
    // SAFETY: a scalar query of this thread's own live window.
    (unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) }) as u32
}

#[allow(
    clippy::cast_possible_wrap,
    reason = "the window style is a 32-bit DWORD stored as a sign-extended LONG"
)]
fn set_style(hwnd: HWND, style: u32) {
    // SAFETY: changing this thread's own live window's style.
    unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, style as i32 as isize) };
}

thread_local! {
    static FULL: RefCell<Option<Saved>> = const { RefCell::new(None) };
    static HOOK: Cell<Option<HHOOK>> = const { Cell::new(None) };
    static TARGET: Cell<Option<HWND>> = const { Cell::new(None) };
    /// Set by the pump each turn: in front, controlling, no local editor.
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    /// Physical keys (scan code + 256 for extended) the hook took while down,
    /// so an auto-repeat is marked as one and its release also goes to us.
    static DOWN: RefCell<[bool; 512]> = const { RefCell::new([false; 512]) };
    /// Ctrl, Alt and Shift as the hook saw them (bits 0, 1, 2).
    static MODIFIERS: Cell<u8> = const { Cell::new(0) };
    /// Enter was taken as the local full-screen shortcut; its release is too.
    static TOGGLE_HELD: Cell<bool> = const { Cell::new(false) };
}

/// Removes the hook and forgets full-screen state when the viewing command
/// ends, on every return path.
pub(super) struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        uninstall();
        FULL.with(|full| *full.borrow_mut() = None);
        MODIFIERS.with(|m| m.set(0));
        TOGGLE_HELD.with(|held| held.set(false));
    }
}

pub(super) fn is_full() -> bool {
    FULL.with(|full| full.borrow().is_some())
}

/// Enter full screen on the window's monitor, or leave it.
pub(super) fn toggle(hwnd: HWND) -> bool {
    if is_full() { leave(hwnd) } else { enter(hwnd) }
}

pub(super) fn enter(hwnd: HWND) -> bool {
    if is_full() {
        return false;
    }
    // SAFETY: scalar queries and style/position changes of this thread's own
    // live window; the info and rectangle are writable locals.
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: u32::try_from(size_of::<MONITORINFO>()).unwrap_or(0),
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &raw mut info).as_bool() {
            return false;
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &raw mut rect).is_err() {
            return false;
        }
        let style = style_of(hwnd);
        let menu = GetMenu(hwnd);
        let _ = SetMenu(hwnd, None);
        let framed = WS_OVERLAPPEDWINDOW.0 | WS_MAXIMIZE.0;
        let frame = style & framed;
        set_style(hwnd, (style & !framed) | WS_POPUP.0);
        let m = info.rcMonitor;
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            m.left,
            m.top,
            m.right - m.left,
            m.bottom - m.top,
            SWP_FRAMECHANGED | SWP_NOOWNERZORDER,
        );
        FULL.with(|full| *full.borrow_mut() = Some(Saved { frame, rect, menu }));
    }
    true
}

pub(super) fn leave(hwnd: HWND) -> bool {
    let Some(saved) = FULL.with(|full| full.borrow_mut().take()) else {
        return false;
    };
    // SAFETY: restoring this thread's own window's earlier style, menu
    // (still owned by the viewer's menu objects) and placement.
    unsafe {
        set_style(hwnd, (style_of(hwnd) & !WS_POPUP.0) | saved.frame);
        let _ = SetMenu(hwnd, Some(saved.menu));
        let r = saved.rect;
        let _ = SetWindowPos(
            hwnd,
            None,
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOOWNERZORDER,
        );
    }
    true
}

/// Install the hook for this window (once). It only acts while [`set_active`].
pub(super) fn install(hwnd: HWND) -> bool {
    if HOOK.with(Cell::get).is_some() {
        return true;
    }
    TARGET.with(|target| target.set(Some(hwnd)));
    // SAFETY: a low-level hook on this thread, which pumps messages; the
    // procedure touches only this thread's state and posts to its window.
    let hook = unsafe {
        GetModuleHandleW(None).and_then(|module| {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), Some(module.into()), 0)
        })
    };
    match hook {
        Ok(hook) => {
            HOOK.with(|slot| slot.set(Some(hook)));
            true
        }
        Err(_) => false,
    }
}

pub(super) fn uninstall() {
    ACTIVE.with(|active| active.set(false));
    if let Some(hook) = HOOK.with(Cell::take) {
        // SAFETY: the hook this thread installed.
        let _ = unsafe { UnhookWindowsHookEx(hook) };
    }
    TARGET.with(|target| target.set(None));
    DOWN.with(|down| *down.borrow_mut() = [false; 512]);
}

pub(super) fn set_active(active: bool) {
    ACTIVE.with(|slot| slot.set(active));
}

fn modifier_bit(vk: u32) -> u8 {
    match u16::try_from(vk).unwrap_or(0) {
        v if v == VK_LCONTROL.0 || v == VK_RCONTROL.0 => 1,
        v if v == VK_LMENU.0 || v == VK_RMENU.0 => 2,
        v if v == VK_LSHIFT.0 || v == VK_RSHIFT.0 => 4,
        _ => 0,
    }
}

extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let pass = || {
        // SAFETY: forwarding this hook call unchanged.
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    };
    if u32::try_from(code) != Ok(HC_ACTION) || lparam.0 == 0 {
        return pass();
    }
    // SAFETY: for HC_ACTION, lParam points to the system's KBDLLHOOKSTRUCT
    // for the duration of this call.
    let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let up = info.flags.0 & LLKHF_UP.0 != 0;
    let extended = info.flags.0 & LLKHF_EXTENDED.0 != 0;
    let bit = modifier_bit(info.vkCode);
    if bit != 0 {
        MODIFIERS.with(|m| m.set(if up { m.get() & !bit } else { m.get() | bit }));
    }
    let Some(target) = TARGET.with(Cell::get) else {
        return pass();
    };
    // SAFETY: a scalar query.
    if unsafe { GetForegroundWindow() } != target {
        return pass();
    }
    // Ctrl+Alt+Enter: the local full-screen shortcut, never sent.
    if info.vkCode == u32::from(VK_RETURN.0) {
        let chord = MODIFIERS.with(Cell::get) & 3 == 3;
        if !up && chord {
            TOGGLE_HELD.with(|held| held.set(true));
            // SAFETY: posting to this thread's own live window.
            let _ = unsafe { PostMessageW(Some(target), TOGGLE_FULLSCREEN, WPARAM(0), LPARAM(0)) };
            return LRESULT(1);
        }
        if up && TOGGLE_HELD.with(|held| held.replace(false)) {
            return LRESULT(1);
        }
    }
    let Ok(scan) = u8::try_from(info.scanCode) else {
        return pass();
    };
    let slot = usize::from(scan) + if extended { 256 } else { 0 };
    let was_down = DOWN.with(|down| down.borrow()[slot]);
    // Keys pressed before the hook became active finish with Windows.
    if !(ACTIVE.with(Cell::get) || (up && was_down)) {
        return pass();
    }
    DOWN.with(|down| down.borrow_mut()[slot] = !up);
    let mut bits: isize = 1 | (isize::from(scan) << 16);
    if extended {
        bits |= 1 << 24;
    }
    if up {
        bits |= (1 << 30) | (1 << 31);
    } else if was_down {
        bits |= 1 << 30; // an auto-repeat; the host repeats held keys itself
    }
    let message = if up { WM_KEYUP } else { WM_KEYDOWN };
    // SAFETY: posting to this thread's own live window; the window procedure
    // reads only the scan code, extended flag and repeat bit from lParam.
    let _ = unsafe {
        PostMessageW(
            Some(target),
            message,
            WPARAM(info.vkCode as usize),
            LPARAM(bits),
        )
    };
    LRESULT(1)
}
