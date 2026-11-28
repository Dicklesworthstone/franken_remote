//! Local framing of one retained native-sized picture. Menu commands carry
//! the opening presentation and attempt; they cannot choose a host region,
//! request input authority, or follow a replaced view.
use crate::{
    control::Choice,
    geometry::{Framing, Presentation, ViewAction},
};
use windows::{
    Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, DestroyMenu, DrawMenuBar, GetMenu, HMENU, MF_BYCOMMAND,
            MF_CHECKED, MF_ENABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MF_UNCHECKED,
            ModifyMenuW,
        },
    },
    core::{HSTRING, Result, w},
};

const FIRST: usize = 0x46b0;
const STATUS: usize = FIRST + 7;
const ACTIONS: [(ViewAction, &str); 7] = [
    (ViewAction::Fit, "&Fit in window"),
    (ViewAction::ActualPixels, "&Actual pixels"),
    (ViewAction::Left, "Pan &left"),
    (ViewAction::Right, "Pan &right"),
    (ViewAction::Up, "Pan &up"),
    (ViewAction::Down, "Pan &down"),
    (ViewAction::Center, "&Center image"),
];

pub(super) fn is_command(id: usize) -> bool {
    (FIRST..STATUS).contains(&id)
}

pub(super) struct Menu {
    popup: HMENU,
    opened: Option<Choice<(u64, Presentation, u64)>>,
}

impl Menu {
    /// Only --actual-pixels launches install this menu. The native window
    /// owns the attached popup and destroys it with its other menus.
    pub(super) fn install(hwnd: HWND) -> Result<Self> {
        // SAFETY: these menu handles belong to this UI thread. Ownership of
        // the popup transfers only after successful attachment to the bar.
        let popup = unsafe {
            let popup = CreatePopupMenu()?;
            let result = (|| {
                for (index, (_, label)) in ACTIONS.iter().enumerate() {
                    if index == 2 {
                        AppendMenuW(popup, MF_SEPARATOR, 0, w!(""))?;
                    }
                    AppendMenuW(
                        popup,
                        MF_STRING | MF_GRAYED,
                        FIRST + index,
                        &HSTRING::from(*label),
                    )?;
                }
                AppendMenuW(popup, MF_SEPARATOR, 0, w!(""))?;
                AppendMenuW(
                    popup,
                    MF_STRING | MF_GRAYED,
                    STATUS,
                    w!("Waiting for a picture"),
                )?;
                AppendMenuW(GetMenu(hwnd), MF_POPUP, popup.0 as usize, w!("&View"))
            })();
            if let Err(error) = result {
                let _ = DestroyMenu(popup);
                return Err(error);
            }
            let _ = DrawMenuBar(hwnd);
            popup
        };
        Ok(Self {
            popup,
            opened: None,
        })
    }

    pub(super) fn is_popup(&self, handle: usize) -> bool {
        self.popup.0 as usize == handle
    }

    pub(super) fn opened(
        &mut self,
        current: Option<(u64, u64, Presentation)>,
        framing: Framing,
        lifetime: u64,
    ) {
        // Capture before native menu updates can dispatch a lifecycle event.
        let opened = super::input_ticks();
        self.opened = current.map(|(attempt, _, presentation)| {
            Choice::new((attempt, presentation, lifetime), opened)
        });
        let picture = current.map(|(_, _, presentation)| presentation);
        for (index, (action, label)) in ACTIONS.iter().enumerate() {
            let enabled = picture.is_some_and(|presentation| match action {
                ViewAction::Fit => true,
                ViewAction::ActualPixels => presentation.native_picture(),
                _ => {
                    let mut next = framing;
                    next.apply(*action, presentation)
                }
            });
            let checked = picture.is_some_and(|presentation| match action {
                ViewAction::Fit => !presentation.actual_pixels(),
                ViewAction::ActualPixels => presentation.actual_pixels(),
                _ => false,
            });
            let flags = MF_BYCOMMAND
                | MF_STRING
                | if enabled { MF_ENABLED } else { MF_GRAYED }
                | if checked { MF_CHECKED } else { MF_UNCHECKED };
            // SAFETY: bounded command ids on this window's live popup. Win32
            // copies labels; no session or image lock spans a native call.
            let _ = unsafe {
                ModifyMenuW(
                    self.popup,
                    u32::try_from(FIRST + index).expect("bounded menu ID"),
                    flags,
                    FIRST + index,
                    &HSTRING::from(*label),
                )
            };
        }
        let label = match picture {
            None => "Waiting for a picture",
            Some(presentation) if !presentation.native_picture() => {
                "Host supplied a scaled picture; native pixels unavailable"
            }
            Some(_) => "Local view only; the host still sends the full display",
        };
        // SAFETY: the same owned popup and copied text as above.
        let _ = unsafe {
            ModifyMenuW(
                self.popup,
                u32::try_from(STATUS).expect("bounded menu ID"),
                MF_BYCOMMAND | MF_STRING | MF_GRAYED,
                STATUS,
                &HSTRING::from(label),
            )
        };
    }

    pub(super) fn take(
        &mut self,
        id: usize,
        current: Option<(u64, u64, Presentation)>,
        lifetime: u64,
        message: u32,
    ) -> Option<ViewAction> {
        let action = ACTIONS.get(id.checked_sub(FIRST)?)?.0;
        let ((attempt, presentation, opened_lifetime), _) =
            self.opened.take()?.take(message, super::input_ticks())?;
        let (current_attempt, _, current_presentation) = current?;
        (attempt != 0
            && attempt == current_attempt
            && presentation == current_presentation
            && opened_lifetime == lifetime)
            .then_some(action)
    }
}
