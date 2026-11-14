//! A modeless choice from the original host-approved display catalog.
//! Only metadata is shown; decoder startup waits in the existing bounded flow.
use super::{current_message_time, input_ticks, message_age};
use fr_mobile_ffi::session::{DisplayRequest, MAX_DISPLAY_CHOICES, Shared};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    rc::{Rc, Weak},
    sync::Arc,
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{COLOR_WINDOW, GetSysColorBrush},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::GetDpiForWindow,
            Input::KeyboardAndMouse::SetFocus,
            WindowsAndMessaging::{
                BN_CLICKED, CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowRect, HMENU,
                IDC_ARROW, LoadCursorW, RegisterClassExW, SW_SHOW, ShowWindow, WINDOW_EX_STYLE,
                WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WNDCLASSEXW, WS_CAPTION, WS_CHILD, WS_POPUP,
                WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
            },
        },
    },
    core::{HSTRING, PCWSTR, Result, w},
};

const ROW: usize = 0x4680;
const ROW_LIMIT: usize = MAX_DISPLAY_CHOICES;
const CANCEL: usize = ROW + ROW_LIMIT;

struct Choice {
    original: Arc<Shared>,
    request: DisplayRequest,
    hwnd: HWND,
    opened: Cell<u64>,
    spent: Cell<bool>,
}

impl Choice {
    fn current(&self) -> bool {
        !self.spent.get() && self.original.display_request_current(&self.request)
    }

    fn cancel(&self) {
        if !self.spent.replace(true) {
            // The exact request check also prevents an old native callback
            // from closing a new attempt that happens to have the same rows.
            let _ = self.original.cancel_display(&self.request);
        }
    }

    fn choose(&self, row: usize, queued: u32) {
        if self.spent.replace(true) {
            return;
        }
        let handle = self
            .request
            .displays()
            .get(row)
            .map(|display| display.handle);
        let recent = message_age(queued).is_ok_and(|(at, _)| at > self.opened.get());
        if recent
            && let Some(handle) = handle
            && self.original.select_display(&self.request, handle).is_ok()
        {
            return;
        }
        // Refuse the whole original choice; never retry it against a new
        // catalog, and never cancel a replacement request on stale failure.
        let _ = self.original.cancel_display(&self.request);
    }
}

thread_local! {
    // A native callback can run while Viewer is borrowed by creation or draw.
    // Choice admission/cancellation never depends on borrowing that renderer.
    static ACTIVE: RefCell<Option<Weak<Choice>>> = const { RefCell::new(None) };
}

fn active(hwnd: HWND) -> Option<Rc<Choice>> {
    ACTIVE.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(Weak::upgrade)
            .filter(|choice| choice.hwnd == hwnd)
    })
}

/// Parent closure is terminal even while a native callback borrows Viewer.
pub(super) fn close_owner() {
    ACTIVE.with(|slot| {
        if let Some(choice) = slot.borrow().as_ref().and_then(Weak::upgrade) {
            choice.original.close();
        }
    });
}

pub(super) struct Picker {
    choice: Rc<Choice>,
}

impl Picker {
    #[allow(
        clippy::too_many_lines,
        reason = "one bounded native picker construction"
    )]
    pub(super) fn open(
        owner: HWND,
        original: Arc<Shared>,
        request: DisplayRequest,
    ) -> Result<Self> {
        // SAFETY: the owner, picker and every child belong to this UI thread.
        // All text is copied by Win32; no payload pointer escapes a call.
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("FrankenRemoteDisplayPicker");
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
            let dpi = GetDpiForWindow(owner).clamp(96, 384);
            let scale = |px: i32| px * i32::try_from(dpi).unwrap_or(96) / 96;
            let mut bounds = RECT::default();
            GetWindowRect(owner, &raw mut bounds)?;
            let rows = i32::try_from(request.displays().len()).unwrap_or(0);
            let height = 152 + rows * 38;
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!("Choose remote display — FrankenRemote"),
                WS_POPUP | WS_CAPTION | WS_SYSMENU,
                bounds.left + scale(24),
                bounds.top + scale(48),
                scale(600),
                scale(height),
                Some(owner),
                None,
                Some(instance.into()),
                None,
            )?;
            let picker = Self {
                choice: Rc::new(Choice {
                    original,
                    request,
                    hwnd,
                    opened: Cell::new(0),
                    spent: Cell::new(false),
                }),
            };
            ACTIVE.with(|slot| *slot.borrow_mut() = Some(Rc::downgrade(&picker.choice)));
            // Picker already owns the parent: every later error destroys all
            // partially created controls through Drop.
            let control = |class, label, style, id, (x, y, width, height)| {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    class,
                    label,
                    WS_CHILD | WS_VISIBLE | style,
                    scale(x),
                    scale(y),
                    scale(width),
                    scale(height),
                    Some(hwnd),
                    Some(HMENU(id as *mut c_void)),
                    Some(instance.into()),
                    None,
                )
            };
            let _ = control(
                w!("STATIC"),
                w!("Choose one display shared by this host. Cancel ends this connection."),
                WINDOW_STYLE(0),
                0,
                (12, 12, 568, 28),
            )?;
            let mut first = None;
            for (row, display) in picker.choice.request.displays().iter().enumerate() {
                let y = 46 + i32::try_from(row).unwrap_or(0) * 38;
                let label = HSTRING::from(format!(
                    "Display {}: {} × {} at ({}, {}), scale {}/{}, rotation {}°",
                    row + 1,
                    display.pixel_width,
                    display.pixel_height,
                    display.x,
                    display.y,
                    display.scale_numerator,
                    display.scale_denominator,
                    u16::from(display.rotation) * 90,
                ));
                let button = control(
                    w!("BUTTON"),
                    PCWSTR(label.as_ptr()),
                    WS_TABSTOP,
                    ROW + row,
                    (12, y, 568, 30),
                )?;
                first.get_or_insert(button);
            }
            let _ = control(
                w!("STATIC"),
                w!("The original connection timeout continues while choosing."),
                WINDOW_STYLE(0),
                0,
                (12, 50 + rows * 38, 568, 24),
            )?;
            let cancel = control(
                w!("BUTTON"),
                w!("&Cancel"),
                WS_TABSTOP,
                CANCEL,
                (460, 78 + rows * 38, 120, 28),
            )?;
            picker.choice.opened.set(input_ticks());
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetFocus(Some(first.unwrap_or(cancel)));
            Ok(picker)
        }
    }

    pub(super) fn window(&self) -> HWND {
        self.choice.hwnd
    }

    pub(super) fn current(&self) -> bool {
        self.choice.current()
    }
}

impl Drop for Picker {
    fn drop(&mut self) {
        ACTIVE.with(|slot| {
            let mut active = slot.borrow_mut();
            if active
                .as_ref()
                .is_some_and(|choice| choice.ptr_eq(&Rc::downgrade(&self.choice)))
            {
                *active = None;
            }
        });
        // SAFETY: this owner-created picker and its children belong only to
        // this UI thread. Its procedure never posts the app's quit message.
        let _ = unsafe { DestroyWindow(self.choice.hwnd) };
    }
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_COMMAND => {
            let id = wparam.0 & 0xffff;
            let notice = u32::try_from((wparam.0 >> 16) & 0xffff).unwrap_or(0);
            if notice == BN_CLICKED
                && let Some(choice) = active(hwnd)
            {
                if (ROW..ROW + ROW_LIMIT).contains(&id) {
                    choice.choose(id - ROW, current_message_time());
                } else if id == CANCEL || id == 2 {
                    // IsDialogMessage sends IDCANCEL (2) for Escape.
                    choice.cancel();
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            if let Some(choice) = active(hwnd) {
                choice.cancel();
            }
            LRESULT(0)
        }
        _ => {
            // SAFETY: standard processing of this thread's native picker.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}
