//! One native choice from an immutable installed-Tailscale snapshot.
//! No remote probing or session startup happens while this window is open.
use crate::control::Choice;
use fr_client::discovery::picker::{Candidate, Catalog, PathHint};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    rc::{Rc, Weak},
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::{COLOR_WINDOW, GetSysColorBrush},
        System::{LibraryLoader::GetModuleHandleW, SystemInformation::GetTickCount64},
        UI::{
            HiDpi::{
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem,
                SetProcessDpiAwarenessContext,
            },
            Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_RETURN},
            WindowsAndMessaging::{
                BN_CLICKED, BS_DEFPUSHBUTTON, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
                DestroyWindow, DispatchMessageW, GetMessageTime, HMENU, IDC_ARROW,
                IsDialogMessageW, LBN_DBLCLK, LBS_HASSTRINGS, LBS_NOINTEGRALHEIGHT, LBS_NOTIFY,
                LB_ADDSTRING, LB_GETCURSEL, LB_SETHORIZONTALEXTENT, LoadCursorW, MSG,
                MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
                RegisterClassExW, SW_SHOW, SendMessageW, ShowWindow, TranslateMessage,
                WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_KEYDOWN, WM_QUIT,
                WNDCLASSEXW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_HSCROLL, WS_SYSMENU,
                WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
            },
        },
    },
    core::{HSTRING, w},
};

const CONNECT: usize = 1;
const CANCEL: usize = 2;
const MACHINES: usize = 0x4690;
/// A local choice retains bounded metadata for at most two minutes. This is
/// independent of, and precedes, every network/session startup deadline.
const CHOICE_MILLIS: u64 = 120_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    WindowUnavailable,
    Expired,
    StaleChoice,
}

type Outcome = Result<Option<Candidate>, Error>;

struct State {
    hwnd: HWND,
    list: Cell<Option<HWND>>,
    connect: Cell<Option<HWND>>,
    cancel: Cell<Option<HWND>>,
    catalog: Catalog,
    gesture: RefCell<Option<Choice<()>>>,
    deadline: u64,
    result: RefCell<Option<Outcome>>,
}

impl State {
    fn owns_input(&self, hwnd: HWND) -> bool {
        hwnd == self.hwnd
            || [self.list.get(), self.connect.get(), self.cancel.get()].contains(&Some(hwnd))
    }

    fn finish(&self, result: Outcome) {
        // Reentrant cancellation wins over an in-flight native selection read.
        if let Ok(mut slot) = self.result.try_borrow_mut()
            && slot.is_none()
        {
            *slot = Some(result);
        }
    }

    fn choose(&self, queued: u32) {
        if self.result.borrow().is_some() {
            return;
        }
        if ticks() >= self.deadline {
            self.finish(Err(Error::Expired));
            return;
        }
        let Some(list) = self.list.get() else {
            self.finish(Err(Error::WindowUnavailable));
            return;
        };
        // SAFETY: this UI thread owns a live, single-selection list box. The
        // message returns an ordinal and receives no borrowed buffer.
        let row = unsafe { SendMessageW(list, LB_GETCURSEL, WPARAM(0), LPARAM(0)) }.0;
        let Ok(row) = usize::try_from(row) else {
            // No selection is not a new machine choice. Let the user select.
            return;
        };
        // Native reads may reenter or stall. Preserve the original gesture's
        // age rather than validating against the pre-read sample.
        let now = ticks();
        if now >= self.deadline {
            self.finish(Err(Error::Expired));
            return;
        }
        let gesture = self.gesture.borrow_mut().take();
        if gesture
            .and_then(|mut choice| choice.take(queued, now))
            .is_none()
        {
            self.finish(Err(Error::StaleChoice));
            return;
        }
        // This ordinal is consumed against the exact catalog displayed in
        // this window. No fresh discovery can reinterpret it.
        let result = self
            .catalog
            .get(row)
            .cloned()
            .map(Some)
            .map_err(|_| Error::StaleChoice);
        if ticks() >= self.deadline {
            self.finish(Err(Error::Expired));
        } else {
            self.finish(result);
        }
    }
}

thread_local! {
    static ACTIVE: RefCell<Option<Weak<State>>> = const { RefCell::new(None) };
}

fn active(hwnd: HWND) -> Option<Rc<State>> {
    ACTIVE.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(Weak::upgrade)
            .filter(|state| state.hwnd == hwnd)
    })
}

struct Picker(Rc<State>);

impl Drop for Picker {
    fn drop(&mut self) {
        self.0.finish(Ok(None));
        ACTIVE.with(|slot| *slot.borrow_mut() = None);
        // SAFETY: this thread owns the root window and all of its children.
        // Callback access is removed before native destruction can reenter.
        let _ = unsafe { DestroyWindow(self.0.hwnd) };
    }
}

fn ticks() -> u64 {
    // SAFETY: GetTickCount64 reads the OS uptime clock without pointer arguments.
    unsafe { GetTickCount64() }
}

/// The result owns the original identity tuple. The caller must still build
/// an installed Target and revalidate through the daemon before any dial.
pub(super) fn choose(catalog: Catalog) -> Outcome {
    let picker = open(catalog).map_err(|_| Error::WindowUnavailable)?;
    loop {
        if let Some(result) = picker.0.result.borrow_mut().take() {
            return result;
        }
        if ticks() >= picker.0.deadline {
            return Err(Error::Expired);
        }
        // A flood cannot keep the original metadata indefinitely: dispatch a
        // bounded round, then check completion and the original deadline again.
        for _ in 0..64 {
            let mut message = MSG::default();
            // SAFETY: writable MSG, this thread's message queue, no raw payloads.
            if !unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                break;
            }
            if message.message == WM_QUIT {
                picker.0.finish(Ok(None));
                break;
            }
            let own_key =
                message.message == WM_KEYDOWN && picker.0.owns_input(message.hwnd);
            if own_key && message.wParam.0 == usize::from(VK_ESCAPE.0) {
                picker.0.finish(Ok(None));
            } else if own_key && message.wParam.0 == usize::from(VK_RETURN.0) {
                if picker.0.cancel.get() == Some(message.hwnd) {
                    picker.0.finish(Ok(None));
                } else {
                    picker.0.choose(message.time);
                }
            } else {
                // SAFETY: live UI-thread window and the initialized queue MSG.
                unsafe {
                    if !IsDialogMessageW(picker.0.hwnd, &raw const message).as_bool() {
                        let _ = TranslateMessage(&raw const message);
                        DispatchMessageW(&raw const message);
                    }
                }
            }
            if picker.0.result.borrow().is_some() || ticks() >= picker.0.deadline {
                break;
            }
        }
        if picker.0.result.borrow().is_none() {
            // SAFETY: no handles or borrowed buffers; wait only for this UI
            // thread's input, with a bounded timeout to service expiry.
            unsafe { MsgWaitForMultipleObjects(None, false, 100, QS_ALLINPUT) };
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one bounded native window construction"
)]
fn open(catalog: Catalog) -> windows::core::Result<Picker> {
    // SAFETY: every window/control is created, used, and destroyed on this
    // thread. Win32 copies all strings synchronously. Picker owns the root
    // before any fallible child creation, so errors destroy the complete tree.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(None)?;
        let class = w!("FrankenRemoteMachinePicker");
        let wc = WNDCLASSEXW {
            cbSize: u32::try_from(size_of::<WNDCLASSEXW>()).unwrap_or(0),
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&raw const wc);
        let dpi = GetDpiForSystem().clamp(96, 384);
        let scale = |px: i32| px * i32::try_from(dpi).unwrap_or(96) / 96;
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("Choose a machine — FrankenRemote"),
            WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            scale(780),
            scale(480),
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        let opened = ticks();
        let picker = Picker(Rc::new(State {
            hwnd,
            list: Cell::new(None),
            connect: Cell::new(None),
            cancel: Cell::new(None),
            catalog,
            gesture: RefCell::new(Some(Choice::new((), opened))),
            deadline: opened.saturating_add(CHOICE_MILLIS),
            result: RefCell::new(None),
        }));
        ACTIVE.with(|slot| *slot.borrow_mut() = Some(Rc::downgrade(&picker.0)));
        let description = if picker.0.catalog.is_empty() {
            w!("No eligible Tailscale machines. Check Tailscale and try again.")
        } else {
            w!("Choose a machine from Tailscale. Desktop availability is checked when you connect.")
        };
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            description,
            WS_CHILD | WS_VISIBLE,
            scale(20),
            scale(18),
            scale(730),
            scale(40),
            Some(hwnd),
            None,
            Some(instance.into()),
            None,
        )?;
        let list_style = WINDOW_STYLE(
            u32::try_from(LBS_NOTIFY | LBS_HASSTRINGS | LBS_NOINTEGRALHEIGHT).unwrap_or(0),
        );
        let list = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("LISTBOX"),
            w!("Tailscale machines"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | WS_VSCROLL | WS_HSCROLL | list_style,
            scale(20),
            scale(64),
            scale(730),
            scale(290),
            Some(hwnd),
            Some(HMENU(MACHINES as *mut c_void)),
            Some(instance.into()),
            None,
        )?;
        picker.0.list.set(Some(list));
        for (index, peer) in picker.0.catalog.candidates().iter().enumerate() {
            let hint = match peer.path_hint() {
                PathHint::Direct => "direct path sampled",
                PathHint::Relayed => "relayed path sampled",
                PathHint::Unknown => "path not sampled",
            };
            let label = HSTRING::from(format!("{}  —  {hint}", peer.certificate_name()));
            let added = SendMessageW(
                list,
                LB_ADDSTRING,
                WPARAM(0),
                LPARAM(label.as_ptr() as isize),
            );
            if usize::try_from(added.0).ok() != Some(index) {
                return Err(windows::core::Error::from_win32());
            }
        }
        SendMessageW(
            list,
            LB_SETHORIZONTALEXTENT,
            WPARAM(usize::try_from(scale(2800)).unwrap_or(2800)),
            LPARAM(0),
        );
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("Select a row, then Connect or Enter. No desktop has been opened. Choice expires in two minutes."),
            WS_CHILD | WS_VISIBLE,
            scale(20),
            scale(365),
            scale(730),
            scale(30),
            Some(hwnd),
            None,
            Some(instance.into()),
            None,
        )?;
        let connect = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("Connect"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP
                | WINDOW_STYLE(u32::try_from(BS_DEFPUSHBUTTON).unwrap_or(0)),
            scale(500),
            scale(400),
            scale(120),
            scale(30),
            Some(hwnd),
            Some(HMENU(CONNECT as *mut c_void)),
            Some(instance.into()),
            None,
        )?;
        picker.0.connect.set(Some(connect));
        let cancel = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("Cancel"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            scale(630),
            scale(400),
            scale(120),
            scale(30),
            Some(hwnd),
            Some(HMENU(CANCEL as *mut c_void)),
            Some(instance.into()),
            None,
        )?;
        picker.0.cancel.set(Some(cancel));
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetFocus(Some(list));
        Ok(picker)
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if let Some(state) = active(hwnd) {
        if message == WM_CLOSE {
            state.finish(Ok(None));
            return LRESULT(0);
        }
        if message == WM_COMMAND {
            let id = wparam.0 & 0xffff;
            let notification = (wparam.0 >> 16) & 0xffff;
            let source = HWND(lparam.0 as *mut c_void);
            let clicked = usize::try_from(BN_CLICKED).ok() == Some(notification);
            if id == CANCEL && clicked && state.cancel.get() == Some(source) {
                state.finish(Ok(None));
                return LRESULT(0);
            }
            if (id == CONNECT && clicked && state.connect.get() == Some(source))
                || (id == MACHINES
                    && usize::try_from(LBN_DBLCLK).ok() == Some(notification)
                    && state.list.get() == Some(source))
            {
                // SAFETY: native queue timestamp is a value, no borrowed input.
                let queued = unsafe { GetMessageTime() };
                state.choose(u32::from_ne_bytes(queued.to_ne_bytes()));
                return LRESULT(0);
            }
        }
    }
    // SAFETY: unhandled messages retain their original Win32 parameters.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
