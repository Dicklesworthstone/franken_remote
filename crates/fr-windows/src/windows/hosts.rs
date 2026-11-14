//! One native choice from an immutable installed-Tailscale snapshot.
//! No remote probing or session startup happens while this window is open.
use crate::host_picker::Choice;
pub(super) use crate::host_picker::Error;
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
                DestroyWindow, DispatchMessageW, GetMessageTime, HMENU, IDC_ARROW, IsDialogMessageW,
                LBN_DBLCLK, LBS_HASSTRINGS, LBS_NOINTEGRALHEIGHT, LBS_NOTIFY, LB_ADDSTRING,
                LB_GETCURSEL, LB_SETHORIZONTALEXTENT, LoadCursorW, MSG, MsgWaitForMultipleObjects,
                PM_NOREMOVE, PM_REMOVE, PeekMessageW, QS_ALLINPUT, RegisterClassExW, SW_SHOW,
                SendMessageW, ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE,
                WM_COMMAND, WM_DESTROY, WM_KEYDOWN, WM_QUIT, WNDCLASSEXW, WS_BORDER, WS_CAPTION,
                WS_CHILD, WS_HSCROLL, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
            },
        },
    },
    core::{HSTRING, w},
};

const CONNECT: usize = 1;
const CANCEL: usize = 2;
const MACHINES: usize = 0x4690;
type Outcome = Result<Option<Candidate>, Error>;

struct State {
    hwnd: HWND,
    list: Cell<Option<HWND>>,
    connect: Cell<Option<HWND>>,
    cancel: Cell<Option<HWND>>,
    choice: RefCell<Choice>,
    retiring: Cell<bool>,
    closed: Cell<bool>,
}

impl State {
    fn owns_input(&self, hwnd: HWND) -> bool {
        hwnd == self.hwnd
            || [self.list.get(), self.connect.get(), self.cancel.get()].contains(&Some(hwnd))
    }

    fn refuse(&self, error: Error) {
        // No choice borrow spans a native call. Reentrant cancellation can
        // invalidate a row until successful destruction and final consumption.
        self.choice.borrow_mut().refuse(error);
    }

    fn choose(&self, queued: u32) {
        if self.choice.borrow_mut().complete(ticks()) {
            return;
        }
        let Some(list) = self.list.get() else {
            self.refuse(Error::NativeUnavailable);
            return;
        };
        // SAFETY: this UI thread owns a live, single-selection list box. The
        // message returns an ordinal and receives no borrowed buffer.
        let row = unsafe { SendMessageW(list, LB_GETCURSEL, WPARAM(0), LPARAM(0)) }.0;
        let Ok(row) = usize::try_from(row) else {
            // No selection is not a new machine choice. Let the user select.
            return;
        };
        // Native reads may reenter or stall. Recheck the original event time,
        // window boundary, snapshot and deadline after the read has completed.
        self.choice.borrow_mut().choose(row, queued, ticks());
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

impl Picker {
    fn close(&self) -> Result<(), Error> {
        if self.0.closed.get() {
            return Ok(());
        }
        self.0.retiring.set(true);
        // SAFETY: this UI thread owns the root and children. Keep the weak
        // callback owner available during destruction so a reentrant close
        // still cancels. WM_DESTROY itself is expected during this retirement.
        if unsafe { DestroyWindow(self.0.hwnd) }.is_err() {
            self.0.refuse(Error::CleanupIncomplete);
            return Err(Error::CleanupIncomplete);
        }
        self.0.closed.set(true);
        ACTIVE.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some_and(|state| Rc::ptr_eq(&state, &self.0))
            {
                *slot = None;
            }
        });
        Ok(())
    }
}

impl Drop for Picker {
    fn drop(&mut self) {
        self.0.refuse(Error::Cancelled);
        // Partial setup and error paths also own cleanup. A successful choice
        // is published only by the explicit, checked close below.
        let _ = self.close();
    }
}

pub(super) fn ticks() -> u64 {
    // SAFETY: GetTickCount64 reads the OS uptime clock without pointer arguments.
    unsafe { GetTickCount64() }
}

/// The result owns the original identity tuple. The caller must still build
/// an installed Target and revalidate through the daemon before any dial.
pub(super) fn choose(catalog: Catalog, started: u64) -> Outcome {
    let choice = Choice::new(catalog, started)?;
    let picker = open(choice).map_err(|_| Error::NativeUnavailable)?;
    loop {
        // Finish a bounded batch even after Connect. A cancellation queued
        // behind that click must still invalidate the provisional selection.
        for _ in 0..64 {
            let mut message = MSG::default();
            // SAFETY: writable MSG, this thread's message queue, no raw payloads.
            if !unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                break;
            }
            if message.message == WM_QUIT {
                picker.0.refuse(Error::Cancelled);
                continue;
            }
            let own_key = message.message == WM_KEYDOWN && picker.0.owns_input(message.hwnd);
            if own_key && message.wParam.0 == usize::from(VK_ESCAPE.0) {
                picker.0.refuse(Error::Cancelled);
            } else if own_key && message.wParam.0 == usize::from(VK_RETURN.0) {
                if picker.0.cancel.get() == Some(message.hwnd) {
                    picker.0.refuse(Error::Cancelled);
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
        }
        if picker.0.choice.borrow_mut().complete(ticks()) {
            let mut pending = MSG::default();
            // SAFETY: writable MSG, no removal or borrowed message payload.
            // Never let Connect overtake a close hidden behind a queue flood.
            if unsafe { PeekMessageW(&raw mut pending, None, 0, 0, PM_NOREMOVE) }.as_bool() {
                picker.0.refuse(Error::EventFlood);
            }
            break;
        }
        // SAFETY: no handles or borrowed buffers; wait only for this UI
        // thread's input, with a bounded timeout to service original expiry.
        unsafe { MsgWaitForMultipleObjects(None, false, 100, QS_ALLINPUT) };
    }
    picker.close()?;
    // Cleanup consumed part of the same original budget. No candidate can
    // leave this owner until destruction succeeded and the clock is rechecked.
    let result = picker.0.choice.borrow_mut().take(ticks());
    match result {
        Ok(peer) => Ok(Some(peer)),
        Err(Error::Cancelled) => Ok(None),
        Err(error) => Err(error),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one bounded native window construction"
)]
fn open(choice: Choice) -> windows::core::Result<Picker> {
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
        let picker = Picker(Rc::new(State {
            hwnd,
            list: Cell::new(None),
            connect: Cell::new(None),
            cancel: Cell::new(None),
            choice: RefCell::new(choice),
            retiring: Cell::new(false),
            closed: Cell::new(false),
        }));
        ACTIVE.with(|slot| *slot.borrow_mut() = Some(Rc::downgrade(&picker.0)));
        let description = if picker.0.choice.borrow().candidates().is_empty() {
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
        let count = picker.0.choice.borrow().candidates().len();
        for index in 0..count {
            let label = {
                let choice = picker.0.choice.borrow();
                let peer = &choice.candidates()[index];
                let hint = match peer.path_hint() {
                    PathHint::Direct => "direct path sampled",
                    PathHint::Relayed => "relayed path sampled",
                    PathHint::Unknown => "path not sampled",
                };
                HSTRING::from(format!("{}  —  {hint}", peer.certificate_name()))
            };
            // The policy borrow ends before this native call can reenter.
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
            w!("Select a row, then Connect or Enter. No desktop has been opened. Choice expires within two minutes."),
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
        picker.0.choice.borrow_mut().shown(ticks());
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
            state.refuse(Error::Cancelled);
            return LRESULT(0);
        }
        if message == WM_DESTROY {
            if !state.retiring.get() {
                state.refuse(Error::NativeUnavailable);
            }
            return LRESULT(0);
        }
        if message == WM_COMMAND {
            let id = wparam.0 & 0xffff;
            let notification = (wparam.0 >> 16) & 0xffff;
            let source = HWND(lparam.0 as *mut c_void);
            let clicked = usize::try_from(BN_CLICKED).ok() == Some(notification);
            if id == CANCEL && clicked && state.cancel.get() == Some(source) {
                state.refuse(Error::Cancelled);
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
