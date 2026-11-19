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
//!
//! The hook runs on its own high-priority thread, never the window's: Windows
//! skips a low-level hook that does not answer within its timeout, and since
//! Windows 7 silently removes it for good, so a busy UI thread (drawing a
//! large picture) would make shortcuts stop working. That thread also
//! registers the hook afresh every two seconds, so even a removal cannot last.
use std::{
    cell::{Cell, RefCell},
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicBool, AtomicIsize, Ordering},
        mpsc,
    },
    thread::JoinHandle,
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow},
    System::{
        LibraryLoader::GetModuleHandleW,
        Threading::{
            GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
        },
    },
    UI::{
        Input::KeyboardAndMouse::{
            VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_RCONTROL, VK_RETURN, VK_RMENU, VK_RSHIFT,
        },
        WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, GWL_STYLE, GetForegroundWindow, GetMenu, GetMessageW,
            GetWindowLongPtrW, GetWindowRect, HC_ACTION, HHOOK, HMENU, HWND_TOP, KBDLLHOOKSTRUCT,
            KillTimer, LLKHF_EXTENDED, LLKHF_UP, MSG, PM_NOREMOVE, PeekMessageW, PostMessageW,
            PostThreadMessageW, SWP_FRAMECHANGED, SWP_NOOWNERZORDER, SWP_NOZORDER, SetMenu,
            SetTimer, SetWindowLongPtrW, SetWindowPos, SetWindowsHookExW, TranslateMessage,
            UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_TIMER,
            WM_USER, WS_MAXIMIZE, WS_OVERLAPPEDWINDOW, WS_POPUP,
        },
    },
};

/// How often the hook thread registers the hook afresh.
const REHOOK_MS: u32 = 2_000;

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
    /// The UI thread's own full-screen state.
    static FULL: RefCell<Option<Saved>> = const { RefCell::new(None) };
    // The rest belongs to the hook thread only (the hook runs there).
    /// Physical keys (scan code + 256 for extended) the hook took while down,
    /// so an auto-repeat is marked as one and its release also goes to us.
    static DOWN: RefCell<[bool; 512]> = const { RefCell::new([false; 512]) };
    /// Ctrl, Alt and Shift as the hook saw them (bits 0, 1, 2).
    static MODIFIERS: Cell<u8> = const { Cell::new(0) };
    /// Enter was taken as the local full-screen shortcut; its release is too.
    static TOGGLE_HELD: Cell<bool> = const { Cell::new(false) };
}

/// The viewer window keys go to (its `HWND` value), 0 for none.
static TARGET: AtomicIsize = AtomicIsize::new(0);
/// Set by the pump each turn: in front, controlling, no local editor.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// The hook thread's id (for its quit message) and handle.
static HOOK_THREAD: Mutex<Option<(u32, JoinHandle<()>)>> = Mutex::new(None);

/// Removes the hook and forgets full-screen state when the viewing command
/// ends, on every return path.
pub(super) struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        uninstall();
        FULL.with(|full| *full.borrow_mut() = None);
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

/// Install the hook for this window (once), on its own thread. It only acts
/// while [`set_active`].
pub(super) fn install(hwnd: HWND) -> bool {
    let mut slot = HOOK_THREAD.lock().unwrap_or_else(PoisonError::into_inner);
    TARGET.store(hwnd.0 as isize, Ordering::SeqCst);
    if slot.is_some() {
        return true;
    }
    let (ready, started) = mpsc::channel();
    let Ok(handle) = std::thread::Builder::new()
        .name("fr-shortcut-hook".into())
        .spawn(move || hook_thread(&ready))
    else {
        TARGET.store(0, Ordering::SeqCst);
        return false;
    };
    if let Ok(Some(thread)) = started.recv() {
        *slot = Some((thread, handle));
        true
    } else {
        let _ = handle.join();
        TARGET.store(0, Ordering::SeqCst);
        false
    }
}

pub(super) fn uninstall() {
    ACTIVE.store(false, Ordering::SeqCst);
    TARGET.store(0, Ordering::SeqCst);
    let running = HOOK_THREAD
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some((thread, handle)) = running {
        // SAFETY: a quit message to the hook thread's own queue, which it
        // created before announcing its id.
        let _ = unsafe { PostThreadMessageW(thread, WM_QUIT, WPARAM(0), LPARAM(0)) };
        let _ = handle.join();
    }
}

pub(super) fn set_active(active: bool) {
    ACTIVE.store(active, Ordering::SeqCst);
}

fn set_hook() -> Option<HHOOK> {
    // SAFETY: a low-level hook owned by the calling hook thread, which pumps
    // messages; the procedure touches only that thread's state, two atomics
    // and posts to the viewer window.
    unsafe {
        GetModuleHandleW(None)
            .and_then(|module| {
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), Some(module.into()), 0)
            })
            .ok()
    }
}

/// The hook's own thread: a message loop that does nothing but run the hook
/// (so it always answers in time) and register it afresh every `REHOOK_MS`.
fn hook_thread(ready: &mpsc::Sender<Option<u32>>) {
    let mut msg = MSG::default();
    // SAFETY: raise this thread's own priority, and create its message queue
    // before announcing its id so a quit message cannot be lost.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        let _ = PeekMessageW(&raw mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
    }
    let Some(mut current) = set_hook() else {
        let _ = ready.send(None);
        return;
    };
    // SAFETY: scalar query of this thread's id.
    let _ = ready.send(Some(unsafe { GetCurrentThreadId() }));
    // SAFETY: a thread timer (no window): WM_TIMER arrives in this queue.
    let timer = unsafe { SetTimer(None, 0, REHOOK_MS, None) };
    loop {
        // SAFETY: the standard message loop on this thread's own queue.
        let got = unsafe { GetMessageW(&raw mut msg, None, 0, 0) };
        if got.0 <= 0 {
            break; // WM_QUIT, or the queue failed
        }
        if msg.message == WM_TIMER && msg.hwnd.0.is_null() {
            // Windows may have removed the hook without telling us. A fresh
            // registration always works; no key can arrive in between, since
            // the hook only runs on this thread.
            if let Some(fresh) = set_hook() {
                // SAFETY: the previous hook this thread registered.
                let _ = unsafe { UnhookWindowsHookEx(current) };
                current = fresh;
            }
            continue;
        }
        // SAFETY: a message this thread just removed from its own queue.
        unsafe {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
    // SAFETY: this thread's own timer and hook.
    unsafe {
        if timer != 0 {
            let _ = KillTimer(None, timer);
        }
        let _ = UnhookWindowsHookEx(current);
    }
    DOWN.with(|down| *down.borrow_mut() = [false; 512]);
    MODIFIERS.with(|m| m.set(0));
    TOGGLE_HELD.with(|held| held.set(false));
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
    let raw = TARGET.load(Ordering::SeqCst);
    if raw == 0 {
        return pass();
    }
    let target = HWND(raw as *mut core::ffi::c_void);
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
    if !(ACTIVE.load(Ordering::SeqCst) || (up && was_down)) {
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
