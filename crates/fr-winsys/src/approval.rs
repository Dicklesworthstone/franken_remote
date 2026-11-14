//! One native consent surface on its CALLER'S retained worker thread. No
//! detached UI thread, authority capability, retry or application input lives
//! here. The caller must recheck its original approval after this returns.
//!
//! Allow requires two mouse edges classified as `IMO_HARDWARE` by Windows.
//! Ordinary `SendInput`, sent/posted commands and keyboard activation cannot
//! approve. Windows also classifies `UIAccess` injection as `IMO_HARDWARE`: this
//! blocks this host's ordinary input sink, not trusted `UIAccess` software
//! or arbitrary malicious code running as the selected local user.
use crate::approval_policy::{Budget, Choice, valid_details};
use std::{cell::RefCell, thread, time::Duration};
use windows::{
    Win32::{
        Foundation::{
            ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, SIZE,
            WPARAM,
        },
        Graphics::Gdi::{
            COLOR_WINDOW, CreateFontIndirectW, DeleteObject, GetDC, GetSysColorBrush,
            GetTextExtentPoint32W, GetTextMetricsW, HFONT, HGDIOBJ, ReleaseDC, SelectObject,
            TEXTMETRICW, UpdateWindow,
        },
        System::{LibraryLoader::GetModuleHandleW, SystemInformation::GetTickCount64},
        UI::{
            HiDpi::{
                AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                GetDpiForWindow, SetThreadDpiAwarenessContext, SystemParametersInfoForDpi,
            },
            Input::{
                GetCurrentInputMessageSource, IMDT_MOUSE, IMO_HARDWARE, INPUT_MESSAGE_SOURCE,
                KeyboardAndMouse::{EnableWindow, GetCapture, SetFocus, VK_ESCAPE, VK_RETURN},
            },
            WindowsAndMessaging::{
                BN_CLICKED, BS_PUSHBUTTON, CreateWindowExW, DefWindowProcW, DestroyWindow,
                DispatchMessageW, GetClientRect, GetForegroundWindow, GetMessageTime,
                GetWindowRect, HMENU, HWND_TOPMOST, IDC_ARROW, IsIconic, IsWindow, IsWindowVisible,
                LoadCursorW, MSG, NONCLIENTMETRICSW, PM_REMOVE, PeekMessageW, RegisterClassExW,
                SIZE_MINIMIZED, SPI_GETNONCLIENTMETRICS, SPI_GETWORKAREA, SW_SHOW, SWP_NOACTIVATE,
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SendMessageW, SetForegroundWindow,
                SetWindowPos, ShowWindow, SystemParametersInfoW, TranslateMessage, WINDOW_EX_STYLE,
                WINDOW_STYLE, WM_ACTIVATEAPP, WM_CANCELMODE, WM_CAPTURECHANGED, WM_CLOSE,
                WM_COMMAND, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ENABLE,
                WM_INPUTLANGCHANGE, WM_KEYDOWN, WM_KEYFIRST, WM_KEYLAST, WM_KILLFOCUS,
                WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOVE,
                WM_NCDESTROY, WM_QUIT, WM_SETFONT, WM_SETTINGCHANGE, WM_SHOWWINDOW, WM_SIZE,
                WNDCLASSEXW, WS_CAPTION, WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
                WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
            },
        },
    },
    core::{HSTRING, PCWSTR, w},
};

const ALLOW: u16 = 101;
const DENY: u16 = 102;
const SUBCLASS: usize = 1;
// winuser.h STATIC styles SS_LEFTNOWORDWRAP | SS_NOPREFIX. Keeping these
// two constants here avoids importing the unrelated SystemServices boundary.
const LABEL_STYLE: WINDOW_STYLE = WINDOW_STYLE(0x0000_000c | 0x0000_0080);
const MESSAGES_PER_TURN: usize = 32;
const TURN: Duration = Duration::from_millis(5);
const INSTRUCTION: &str = "Click Allow for this request only. Enter, Escape or closing denies.";

// The three documented comctl32 subclass helpers, with the exact Win32 ABI
// used by windows 0.62.2's generated Shell bindings. Importing the whole Shell
// feature for these functions also compiles tens of thousands of unrelated
// shell interfaces. No raw pointer crosses this narrow HWND-only boundary.
mod subclass {
    use super::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::core::BOOL;

    type Procedure =
        Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM, usize, usize) -> LRESULT>;

    #[link(name = "comctl32")]
    unsafe extern "system" {
        #[link_name = "SetWindowSubclass"]
        pub(super) fn set(window: HWND, procedure: Procedure, id: usize, data: usize) -> BOOL;
        #[link_name = "RemoveWindowSubclass"]
        pub(super) fn remove(window: HWND, procedure: Procedure, id: usize) -> BOOL;
        #[link_name = "DefSubclassProc"]
        pub(super) fn default(
            window: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Unavailable,
    Cancelled,
    Expired,
    /// The exact native surface could not be confirmed destroyed. The caller
    /// must retain its occupied capacity and must not consume a positive answer.
    Cleanup,
}

struct Local {
    window: HWND,
    allow: HWND,
    choice: Choice,
    shown: bool,
    closing: bool,
}

thread_local! {
    // Only scalar handles and gesture custody; no references to caller stack
    // or authority. No borrow is held across a native call or dispatch.
    static ACTIVE: RefCell<Option<Local>> = const { RefCell::new(None) };
}

struct Activation;
impl Activation {
    fn claim() -> Result<Self, Error> {
        ACTIVE.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_some() {
                return Err(Error::Unavailable);
            }
            *slot = Some(Local {
                window: HWND::default(),
                allow: HWND::default(),
                choice: Choice::default(),
                shown: false,
                closing: false,
            });
            Ok(Self)
        })
    }
}
impl Drop for Activation {
    fn drop(&mut self) {
        ACTIVE.with(|slot| *slot.borrow_mut() = None);
    }
}

fn local<T>(f: impl FnOnce(&mut Local) -> T) -> Option<T> {
    ACTIVE.with(|slot| slot.borrow_mut().as_mut().map(f))
}

/// Show one complete attributed request and return one local choice, only
/// after destroying its window. `check` must inspect the ORIGINAL authority,
/// cancellation and deadline; it is never given a way to extend them. The
/// callback receives `true` throughout opening and `false` afterwards, so its
/// original pre-spawn opening deadline also remains enforceable.
///
/// Runs synchronously on the supplied approval worker, never on an authority
/// or reactor thread. A stuck native call remains owned by that same worker.
pub fn request(
    details: &str,
    control: bool,
    mut check: impl FnMut(bool) -> bool,
) -> Result<bool, Error> {
    let mut budget = Budget::new(crate::time::since_boot_ns());
    if !valid_details(details) {
        return Err(Error::Unavailable);
    }
    let _activation = Activation::claim()?;
    let mut window = Window::default();
    let outcome = window
        .open(details, control, &mut check, &mut budget)
        .and_then(|()| window.wait(&mut check, &mut budget))
        .and_then(|answer| {
            if answer {
                // Last readable-surface proof before destruction; every native
                // query may itself dispatch a nested lifecycle callback.
                window.current(&mut check, &mut budget)?;
                native_answer()?.ok_or(Error::Unavailable)
            } else {
                Ok(false)
            }
        });
    if outcome.is_err() {
        local(|state| state.choice.deny());
    }
    // A native yes is tentative. Destruction and all time it spends precede
    // the last checks here and the caller's original-capability consumption.
    window.close()?;
    guard(&mut check, &mut budget, false)?;
    outcome.and_then(|answer| {
        if answer {
            native_answer()?.ok_or(Error::Unavailable)
        } else {
            Ok(false)
        }
    })
}

fn native_answer() -> Result<Option<bool>, Error> {
    // SAFETY: read the same native uptime domain as the original mouse event,
    // after all native queries/destruction whose delay must be charged to it.
    let now = unsafe { GetTickCount64() };
    local(|state| match state.choice.answer() {
        Some(true) if !state.choice.current_allow(now) => {
            state.choice.deny();
            Err(Error::Expired)
        }
        answer => Ok(answer),
    })
    .unwrap_or(Err(Error::Unavailable))
}

fn guard(
    check: &mut impl FnMut(bool) -> bool,
    budget: &mut Budget,
    opening: bool,
) -> Result<(), Error> {
    if !check(opening) {
        return Err(Error::Cancelled);
    }
    if !budget.check(crate::time::since_boot_ns(), opening) {
        return Err(Error::Expired);
    }
    if !crate::input::input_desktop_is_default() {
        return Err(Error::Unavailable);
    }
    if !check(opening) {
        return Err(Error::Cancelled);
    }
    if !budget.check(crate::time::since_boot_ns(), opening) {
        return Err(Error::Expired);
    }
    Ok(())
}

#[derive(Default)]
struct Window {
    hwnd: Option<HWND>,
    font: Option<HFONT>,
    size: (i32, i32),
}

impl Window {
    #[allow(
        clippy::too_many_lines,
        reason = "one complete measured native surface"
    )]
    fn open(
        &mut self,
        details: &str,
        control: bool,
        check: &mut impl FnMut(bool) -> bool,
        budget: &mut Budget,
    ) -> Result<(), Error> {
        guard(check, budget, true)?;
        let title = if control {
            "Allow remote viewing and control?"
        } else {
            "Allow remote viewing?"
        };
        let lines: Vec<&str> = std::iter::once(title)
            .chain(details.lines())
            .chain(std::iter::once(INSTRUCTION))
            .collect();
        // SAFETY: every HWND belongs to this calling thread. All native text
        // arguments are call-scoped and Windows copies them into the controls.
        unsafe {
            let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let instance: HINSTANCE = GetModuleHandleW(None)
                .map_err(|_| Error::Unavailable)?
                .into();
            let class = w!("FrankenRemoteLocalApproval");
            let wc = WNDCLASSEXW {
                cbSize: u32::try_from(size_of::<WNDCLASSEXW>()).map_err(|_| Error::Unavailable)?,
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                hCursor: LoadCursorW(None, IDC_ARROW).map_err(|_| Error::Unavailable)?,
                hbrBackground: GetSysColorBrush(COLOR_WINDOW),
                lpszClassName: class,
                ..Default::default()
            };
            if RegisterClassExW(&raw const wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS
            {
                return Err(Error::Unavailable);
            }
            let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
            let extended = WS_EX_TOPMOST | WS_EX_TOOLWINDOW;
            let hwnd = CreateWindowExW(
                extended,
                class,
                w!("FrankenRemote approval"),
                style,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance),
                None,
            )
            .map_err(|_| Error::Unavailable)?;
            self.hwnd = Some(hwnd);
            local(|state| state.window = hwnd);
            guard(check, budget, true)?;
            let dpi = GetDpiForWindow(hwnd);
            if !(96..=384).contains(&dpi) {
                return Err(Error::Unavailable);
            }
            let scale = |pixels: i32| pixels * i32::try_from(dpi).unwrap_or(96) / 96;
            let font = message_font(dpi)?;
            self.font = Some(font);
            let (text_width, row) = measure(hwnd, font, &lines)?;
            let margin = scale(16);
            let button_width = scale(150);
            let button_height = row + scale(14);
            let width = (text_width + margin * 2).max(button_width * 2 + margin * 3);
            let rows = i32::try_from(lines.len()).map_err(|_| Error::Unavailable)?;
            let height = rows * row + margin * 4 + button_height;
            self.size = (width, height);
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            };
            AdjustWindowRectExForDpi(&raw mut rect, style, false, extended, dpi)
                .map_err(|_| Error::Unavailable)?;
            let (outer_width, outer_height) = (rect.right - rect.left, rect.bottom - rect.top);
            let area = work_area()?;
            if outer_width > area.right - area.left || outer_height > area.bottom - area.top {
                return Err(Error::Unavailable);
            }
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                area.left + (area.right - area.left - outer_width) / 2,
                area.top + (area.bottom - area.top - outer_height) / 2,
                outer_width,
                outer_height,
                SWP_NOACTIVATE,
            )
            .map_err(|_| Error::Unavailable)?;
            guard(check, budget, true)?;
            for (index, line) in lines.iter().enumerate() {
                let y = margin + i32::try_from(index).map_err(|_| Error::Unavailable)? * row;
                child(
                    hwnd,
                    instance,
                    font,
                    w!("STATIC"),
                    &HSTRING::from(*line),
                    LABEL_STYLE,
                    0,
                    [margin, y, width - margin * 2, row],
                )?;
            }
            let bottom = height - margin - button_height;
            let allow = child(
                hwnd,
                instance,
                font,
                w!("BUTTON"),
                &HSTRING::from(if control {
                    "Allow control"
                } else {
                    "Allow viewing"
                }),
                WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                ALLOW,
                [
                    width - margin * 2 - button_width * 2,
                    bottom,
                    button_width,
                    button_height,
                ],
            )?;
            let deny = child(
                hwnd,
                instance,
                font,
                w!("BUTTON"),
                &HSTRING::from("Deny"),
                WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                DENY,
                [
                    width - margin - button_width,
                    bottom,
                    button_width,
                    button_height,
                ],
            )?;
            let _ = EnableWindow(allow, false);
            local(|state| state.allow = allow);
            if !subclass::set(allow, Some(allow_proc), SUBCLASS, 0).as_bool() {
                return Err(Error::Unavailable);
            }
            guard(check, budget, true)?;
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            let _ = SetFocus(Some(deny));
            if !UpdateWindow(hwnd).as_bool() || !self.visible() {
                return Err(Error::Unavailable);
            }
            guard(check, budget, true)?;
            // The first allowable gesture must postdate the fully drawn
            // surface, not the beginning of native setup or a replacement UI.
            let shown = GetTickCount64();
            local(|state| {
                state.choice.show(shown);
                state.shown = true;
            });
            let _ = EnableWindow(allow, true);
        }
        guard(check, budget, true)
    }

    fn wait(
        &self,
        check: &mut impl FnMut(bool) -> bool,
        budget: &mut Budget,
    ) -> Result<bool, Error> {
        loop {
            self.current(check, budget)?;
            for _ in 0..MESSAGES_PER_TURN {
                let mut message = MSG::default();
                // SAFETY: this worker's own queue; no caller-owned pointers.
                let queued =
                    unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) }.as_bool();
                // PeekMessage can dispatch SENT callbacks even when it returns
                // no queued message. They cannot hide or move this surface and
                // leave a previously captured Allow consumable.
                self.current(check, budget)?;
                if !queued {
                    break;
                }
                if message.message == WM_QUIT {
                    local(|state| state.choice.deny());
                } else if message.message == WM_KEYDOWN
                    && [usize::from(VK_ESCAPE.0), usize::from(VK_RETURN.0)]
                        .contains(&message.wParam.0)
                {
                    local(|state| state.choice.deny());
                } else {
                    if (WM_KEYFIRST..=WM_KEYLAST).contains(&message.message) {
                        local(|state| state.choice.invalidate_gesture());
                    }
                    // SAFETY: the message was removed from this thread's queue;
                    // no ACTIVE borrow crosses synchronous/reentrant callbacks.
                    unsafe {
                        let _ = TranslateMessage(&raw const message);
                        DispatchMessageW(&raw const message);
                    }
                }
                self.current(check, budget)?;
                if let Some(answer) = native_answer()? {
                    return Ok(answer);
                }
            }
            self.current(check, budget)?;
            if let Some(answer) = native_answer()? {
                return Ok(answer);
            }
            thread::sleep(TURN);
        }
    }

    fn current(
        &self,
        check: &mut impl FnMut(bool) -> bool,
        budget: &mut Budget,
    ) -> Result<(), Error> {
        guard(check, budget, false)?;
        if !self.visible() {
            local(|state| state.choice.deny());
            return Err(Error::Unavailable);
        }
        guard(check, budget, false)
    }

    fn visible(&self) -> bool {
        let Some(hwnd) = self.hwnd else {
            return false;
        };
        let mut client = RECT::default();
        let mut outer = RECT::default();
        let Ok(area) = work_area() else {
            return false;
        };
        // SAFETY: this worker owns the window throughout the query batch.
        unsafe {
            IsWindowVisible(hwnd).as_bool()
                && !IsIconic(hwnd).as_bool()
                && GetClientRect(hwnd, &raw mut client).is_ok()
                && GetWindowRect(hwnd, &raw mut outer).is_ok()
                && (client.right - client.left, client.bottom - client.top) == self.size
                && outer.left >= area.left
                && outer.top >= area.top
                && outer.right <= area.right
                && outer.bottom <= area.bottom
        }
    }

    fn close(&mut self) -> Result<(), Error> {
        if let Some(hwnd) = self.hwnd {
            local(|state| {
                state.closing = true;
                state.choice.invalidate_gesture();
            });
            // SAFETY: only the top-level window created by this worker and its
            // owned children are destroyed; never a caller or foreign HWND.
            let destroyed = unsafe { DestroyWindow(hwnd) }.is_ok();
            // SAFETY: a handle query after the same thread's destruction.
            if !destroyed || unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                return Err(Error::Cleanup);
            }
            self.hwnd = None;
        }
        if let Some(font) = self.font.take() {
            // SAFETY: every child using this owned font is already destroyed.
            if !unsafe { DeleteObject(HGDIOBJ(font.0)) }.as_bool() {
                return Err(Error::Cleanup);
            }
        }
        Ok(())
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // Panic/error cleanup still belongs to this exact worker. A failed
        // explicit close already returns Cleanup, even if this retry succeeds.
        let _ = self.close();
    }
}

fn work_area() -> Result<RECT, Error> {
    let mut area = RECT::default();
    // SAFETY: a correctly sized stack out-structure; no retained pointer.
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&raw mut area).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS::default(),
        )
    }
    .map_err(|_| Error::Unavailable)?;
    Ok(area)
}

fn message_font(dpi: u32) -> Result<HFONT, Error> {
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: u32::try_from(size_of::<NONCLIENTMETRICSW>()).map_err(|_| Error::Unavailable)?,
        ..Default::default()
    };
    // SAFETY: sized out-structure, followed by an owned font copied from it.
    unsafe {
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            metrics.cbSize,
            Some((&raw mut metrics).cast()),
            0,
            dpi,
        )
        .map_err(|_| Error::Unavailable)?;
        let font = CreateFontIndirectW(&raw const metrics.lfMessageFont);
        if font.is_invalid() {
            Err(Error::Unavailable)
        } else {
            Ok(font)
        }
    }
}

fn measure(window: HWND, font: HFONT, lines: &[&str]) -> Result<(i32, i32), Error> {
    // SAFETY: this window and font belong to this thread. The DC and previous
    // selected font are restored on every path before returning.
    unsafe {
        let dc = GetDC(Some(window));
        if dc.is_invalid() {
            return Err(Error::Unavailable);
        }
        let previous = SelectObject(dc, HGDIOBJ(font.0));
        let measured = (|| {
            if previous.is_invalid() {
                return Err(Error::Unavailable);
            }
            let mut metrics = TEXTMETRICW::default();
            if !GetTextMetricsW(dc, &raw mut metrics).as_bool() {
                return Err(Error::Unavailable);
            }
            let row = metrics
                .tmHeight
                .checked_add(metrics.tmExternalLeading)
                .filter(|height| *height > 0 && *height <= 512)
                .ok_or(Error::Unavailable)?;
            let mut width = 0;
            for line in lines {
                let units: Vec<u16> = line.encode_utf16().collect();
                let mut size = SIZE::default();
                if !GetTextExtentPoint32W(dc, &units, &raw mut size).as_bool()
                    || size.cx < 0
                    || size.cx > 16_384
                    || size.cy > row
                {
                    return Err(Error::Unavailable);
                }
                width = width.max(size.cx);
            }
            Ok((width, row))
        })();
        let _ = SelectObject(dc, previous);
        let _ = ReleaseDC(Some(window), dc);
        measured
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one Win32 child construction boundary"
)]
fn child(
    parent: HWND,
    instance: HINSTANCE,
    font: HFONT,
    class: PCWSTR,
    text: &HSTRING,
    style: WINDOW_STYLE,
    id: u16,
    rect: [i32; 4],
) -> Result<HWND, Error> {
    // SAFETY: caller owns parent/instance/font on this thread. Native text is
    // copied during CreateWindowExW; the parent owns every returned child.
    unsafe {
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            text,
            WS_CHILD | WS_VISIBLE | style,
            rect[0],
            rect[1],
            rect[2],
            rect[3],
            Some(parent),
            Some(HMENU(usize::from(id) as *mut _)),
            Some(instance),
            None,
        )
        .map_err(|_| Error::Unavailable)?;
        SendMessageW(
            hwnd,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
        Ok(hwnd)
    }
}

fn mouse_source() -> bool {
    let mut source = INPUT_MESSAGE_SOURCE::default();
    // SAFETY: a local out-structure. Must be called during the actual input
    // window procedure, before any sent message changes the current source.
    unsafe { GetCurrentInputMessageSource(&raw mut source) }.is_ok()
        && source.deviceType == IMDT_MOUSE
        && source.originId == IMO_HARDWARE
}

fn mouse_inside(window: HWND, lparam: LPARAM) -> bool {
    let bits = lparam.0.to_ne_bytes();
    let x = i32::from(i16::from_ne_bytes([bits[0], bits[1]]));
    let y = i32::from(i16::from_ne_bytes([bits[2], bits[3]]));
    let mut rect = RECT::default();
    // SAFETY: our subclassed button and a stack out-structure.
    unsafe { GetClientRect(window, &raw mut rect) }.is_ok()
        && x >= rect.left
        && x < rect.right
        && y >= rect.top
        && y < rect.bottom
}

unsafe extern "system" fn allow_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match message {
        WM_LBUTTONDOWN | WM_LBUTTONUP => {
            // Read provenance FIRST, inside dispatch. BN_CLICKED and automation
            // callbacks are never interpreted as approval evidence.
            let source = mouse_source();
            // SAFETY: native counter/message and foreground/capture queries.
            let (stamp, foreground, captured) = unsafe {
                (
                    u32::from_ne_bytes(GetMessageTime().to_ne_bytes()),
                    GetForegroundWindow(),
                    GetCapture(),
                )
            };
            let inside = mouse_inside(window, lparam);
            let ordinary = crate::input::input_desktop_is_default();
            // SAFETY: sample AFTER native queries; their wait is queue age too.
            let now = unsafe { GetTickCount64() };
            local(|state| {
                let qualified = source
                    && ordinary
                    && inside
                    && state.shown
                    && !state.closing
                    && state.allow == window
                    && state.window == foreground;
                if message == WM_LBUTTONDOWN {
                    state.choice.press(stamp, now, qualified);
                } else {
                    state
                        .choice
                        .release(stamp, now, qualified && captured == window);
                }
            });
        }
        WM_MOUSEMOVE if !mouse_source() => {
            local(|state| state.choice.invalidate_gesture());
        }
        WM_CANCELMODE | WM_CAPTURECHANGED | WM_LBUTTONDBLCLK => {
            local(|state| state.choice.invalidate_gesture());
        }
        WM_KILLFOCUS => {
            local(|state| {
                if !state.closing {
                    state.choice.context_changed();
                }
            });
        }
        WM_KEYFIRST..=WM_KEYLAST => {
            local(|state| state.choice.invalidate_gesture());
        }
        WM_NCDESTROY => {
            // SAFETY: removing exactly our subclass from its own dying child.
            let _ = unsafe { subclass::remove(window, Some(allow_proc), SUBCLASS) };
        }
        _ => {}
    }
    // SAFETY: preserve the native BUTTON procedure and subclass chain with
    // the unmodified message received by this installed callback.
    unsafe { subclass::default(window, message, wparam, lparam) }
}

extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let owns = local(|state| state.window == window).unwrap_or(false);
    if owns {
        match message {
            WM_COMMAND => {
                let id = wparam.0 & 0xffff;
                let notice = u32::try_from((wparam.0 >> 16) & 0xffff).unwrap_or(u32::MAX);
                if id == usize::from(DENY) && notice == BN_CLICKED {
                    local(|state| state.choice.deny());
                }
                // ALLOW commands, including native BN_CLICKED, are ignored.
                // Only the qualified mouse callback above can tentatively allow.
                return LRESULT(0);
            }
            WM_CLOSE => {
                local(|state| state.choice.deny());
                return LRESULT(0);
            }
            WM_ACTIVATEAPP if wparam.0 == 0 => {
                local(|state| {
                    if !state.closing {
                        state.choice.deny();
                    }
                });
            }
            WM_SIZE if wparam.0 == SIZE_MINIMIZED as usize => {
                local(|state| state.choice.deny());
            }
            WM_MOVE | WM_SIZE => {
                local(|state| {
                    if state.shown && !state.closing {
                        state.choice.context_changed();
                    }
                });
            }
            WM_SHOWWINDOW | WM_ENABLE if wparam.0 == 0 => {
                local(|state| {
                    if state.shown && !state.closing {
                        state.choice.deny();
                    }
                });
            }
            WM_DESTROY => {
                local(|state| {
                    if !state.closing {
                        state.choice.deny();
                    }
                });
            }
            WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE => {
                local(|state| {
                    if state.shown {
                        state.choice.deny();
                    }
                });
            }
            WM_INPUTLANGCHANGE | WM_CANCELMODE => {
                local(|state| state.choice.invalidate_gesture());
            }
            _ => {}
        }
    }
    // SAFETY: native default handling for this thread's own top-level window.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}
