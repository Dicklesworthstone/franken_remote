//! Visible input ownership and explicit local control choices. The menu's
//! original session is retained independently of the borrowed renderer so
//! Stop fences it immediately even during a nested native callback.
use super::{input_ticks, text};
use crate::control::Choice;
use fr_mobile_ffi::session::{ControlRequest, ControlState, Shared};
use std::{
    cell::RefCell,
    sync::{Arc, Weak},
};
use windows::{
    Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{
            AppendMenuW, CreateMenu, CreatePopupMenu, DestroyMenu, DrawMenuBar, EnableMenuItem,
            HMENU, MF_BYCOMMAND, MF_ENABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, SetMenu,
            SetWindowTextW,
        },
    },
    core::{HSTRING, Result, w},
};

pub(super) const REQUEST: usize = 0x4670;
pub(super) const STOP: usize = 0x4671;

/// Menu command ids are `WM_COMMAND`'s 16-bit low word; `EnableMenuItem` takes
/// them as `u32`.
const fn item(id: usize) -> u32 {
    assert!(id <= 0xffff);
    #[allow(clippy::cast_possible_truncation)] // bounded just above
    {
        id as u32
    }
}

thread_local! {
    static ORIGINAL: RefCell<Option<Weak<Shared>>> = const { RefCell::new(None) };
    static STOP_ORIGINAL: RefCell<Option<Weak<Shared>>> = const { RefCell::new(None) };
}

pub(super) fn bind(original: &Arc<Shared>) {
    ORIGINAL.with(|slot| *slot.borrow_mut() = Some(Arc::downgrade(original)));
}

/// Window lifecycle checks need no mutable access to Viewer or any codec.
pub(super) fn visible(visible: bool) {
    ORIGINAL.with(|slot| {
        if let Some(original) = slot.borrow().as_ref().and_then(Weak::upgrade) {
            original.set_visible(visible);
        }
    });
}

pub(super) fn invalidate_choice() {
    ORIGINAL.with(|slot| {
        if let Some(original) = slot.borrow().as_ref().and_then(Weak::upgrade) {
            original.retire_control_request();
        }
    });
}

/// Stop only the owner from the popup that offered it. A delayed callback
/// cannot close a replacement session or create a control request.
pub(super) fn stop() -> bool {
    let Some(original) =
        STOP_ORIGINAL.with(|slot| slot.borrow_mut().take().and_then(|owner| owner.upgrade()))
    else {
        return false;
    };
    original.release_control();
    ORIGINAL.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|current| current.ptr_eq(&Arc::downgrade(&original)))
    })
}

pub(super) fn close() {
    ORIGINAL.with(|slot| {
        if let Some(original) = slot.borrow().as_ref().and_then(Weak::upgrade) {
            original.close();
        }
    });
}

pub(super) struct Menu {
    hwnd: HWND,
    popup: HMENU,
    title: String,
    displayed: Option<ControlState>,
    choice: Option<Choice<ControlRequest>>,
}

impl Menu {
    /// Ownership of the menu transfers to `hwnd` only after `SetMenu` succeeds.
    pub(super) fn install(hwnd: HWND, title: &str) -> Result<Self> {
        // SAFETY: all handles are created here on this UI thread. Failure
        // destroys only the newly owned menu tree; the window owns it after
        // SetMenu, and remains alive longer than this borrowed popup handle.
        let popup = unsafe {
            let menu = CreateMenu()?;
            let popup = match CreatePopupMenu() {
                Ok(popup) => popup,
                Err(error) => {
                    let _ = DestroyMenu(menu);
                    return Err(error);
                }
            };
            let attached = AppendMenuW(popup, MF_STRING, REQUEST, w!("&Request control"))
                .and_then(|()| {
                    AppendMenuW(popup, MF_STRING, STOP, w!("&Stop control and disconnect"))
                })
                .and_then(|()| AppendMenuW(popup, MF_SEPARATOR, 0, w!("")))
                .and_then(|()| AppendMenuW(popup, MF_STRING, text::OPEN, w!("Send &text…")))
                .and_then(|()| AppendMenuW(menu, MF_POPUP, popup.0 as usize, w!("&Input")));
            if let Err(error) = attached {
                let _ = DestroyMenu(popup);
                let _ = DestroyMenu(menu);
                return Err(error);
            }
            if let Err(error) = SetMenu(hwnd, Some(menu)) {
                let _ = DestroyMenu(menu);
                return Err(error);
            }
            popup
        };
        Ok(Self {
            hwnd,
            popup,
            title: title.to_owned(),
            displayed: None,
            choice: None,
        })
    }

    pub(super) fn is_popup(&self, handle: usize) -> bool {
        self.popup.0 as usize == handle
    }

    pub(super) fn opened(&mut self, original: &Arc<Shared>) {
        self.refresh(original);
        let choice = original.control_request();
        let opened = input_ticks();
        self.choice = choice.map(|request| Choice::new(request, opened));
        STOP_ORIGINAL.with(|slot| *slot.borrow_mut() = Some(Arc::downgrade(original)));
    }

    pub(super) fn request(&mut self, original: &Shared, message: u32) -> bool {
        let Some(mut choice) = self.choice.take() else {
            return false;
        };
        let Some((request, age)) = choice.take(message, input_ticks()) else {
            return false;
        };
        original.request_control_for(&request, age)
    }

    pub(super) fn refresh(&mut self, original: &Shared) {
        let state = original.control_state();
        if self.displayed == Some(state) {
            return;
        }
        self.displayed = Some(state);
        let enabled = |available| if available { MF_ENABLED } else { MF_GRAYED };
        let description = match state {
            ControlState::Unavailable => "View only (start with --control to request input)",
            ControlState::WaitingForView => "Waiting for a ready view",
            ControlState::Viewing => "Viewing — Input → Request control",
            ControlState::Requesting => "Control requested — waiting for host grant",
            ControlState::Controlled => "Controlling — Input → Stop control",
            ControlState::Closing => "Stopping — waiting for cleanup",
            ControlState::Ended => "Session ended",
        };
        // SAFETY: this thread's live menu and window. Text is copied by Win32.
        // UI state is only descriptive; request admission rechecks its original
        // token and control remains exclusively with the shared session owner.
        unsafe {
            let _ = EnableMenuItem(
                self.popup,
                item(REQUEST),
                MF_BYCOMMAND | enabled(state == ControlState::Viewing),
            );
            let _ = EnableMenuItem(
                self.popup,
                item(STOP),
                MF_BYCOMMAND
                    | enabled(matches!(
                        state,
                        ControlState::Requesting | ControlState::Controlled
                    )),
            );
            let _ = EnableMenuItem(
                self.popup,
                item(text::OPEN),
                MF_BYCOMMAND | enabled(state == ControlState::Controlled),
            );
            let _ = SetWindowTextW(
                self.hwnd,
                &HSTRING::from(format!("{} — {description}", self.title)),
            );
            let _ = DrawMenuBar(self.hwnd);
        }
    }
}
