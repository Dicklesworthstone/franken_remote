//! One explicit receive destination on the original controlling connection.
//! Native menu callbacks can only stop that owner and inspect bounded evidence.
use crate::control::Choice;
use fr_mobile_ffi::session::{Download, DownloadReport};
use std::{
    cell::RefCell,
    sync::{Arc, Weak},
};
use windows::{
    Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, DestroyMenu, DrawMenuBar, EnableMenuItem, GetMenu,
            GetMessageTime, HMENU, MF_BYCOMMAND, MF_ENABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR,
            MF_STRING, ModifyMenuW,
        },
    },
    core::{HSTRING, Result, w},
};

pub(super) const CANCEL: usize = 0x4690;
const PHASE: usize = 0x4691;
const BYTES: usize = 0x4692;
const RECEIPTS: usize = 0x4693;
const CLEANUP: usize = 0x4694;

thread_local! {
    // A queued native command retains the original displayed receiver. Viewing
    // replacements cannot redirect it to another destination or grant.
    static ACTION: RefCell<Option<Choice<Weak<Download>>>> = const { RefCell::new(None) };
}

pub(super) fn cancel() {
    // SAFETY: original queued command timestamp on this UI thread.
    let message = unsafe { GetMessageTime() }.cast_unsigned();
    let selected = ACTION.with(|slot| {
        slot.borrow_mut()
            .as_mut()?
            .take(message, super::input_ticks())
    });
    if let Some((owner, _age)) = selected
        && let Some(owner) = owner.upgrade()
    {
        owner.cancel();
    }
}

pub(super) struct Menu {
    hwnd: HWND,
    popup: HMENU,
    original: Arc<Download>,
    displayed: Option<DownloadReport>,
}

impl Menu {
    pub(super) fn install(hwnd: HWND, original: Arc<Download>) -> Result<Self> {
        // SAFETY: this UI thread's existing menu. The window owns the popup
        // only after successful attachment; an unattached popup is destroyed.
        let popup = unsafe {
            let menu = GetMenu(hwnd);
            if menu.is_invalid() {
                return Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_FAIL,
                ));
            }
            let popup = CreatePopupMenu()?;
            let attached = AppendMenuW(popup, MF_STRING | MF_GRAYED, PHASE, w!("Selected"))
                .and_then(|()| AppendMenuW(popup, MF_STRING | MF_GRAYED, BYTES, w!("Staged: none")))
                .and_then(|()| {
                    AppendMenuW(popup, MF_STRING | MF_GRAYED, RECEIPTS, w!("Published: none"))
                })
                .and_then(|()| {
                    AppendMenuW(
                        popup,
                        MF_STRING | MF_GRAYED,
                        CLEANUP,
                        w!("Cleanup: not started"),
                    )
                })
                .and_then(|()| AppendMenuW(popup, MF_SEPARATOR, 0, w!("")))
                .and_then(|()| AppendMenuW(popup, MF_STRING, CANCEL, w!("&Cancel receiving")))
                .and_then(|()| AppendMenuW(menu, MF_POPUP, popup.0 as usize, w!("&Receive files")));
            if let Err(error) = attached {
                let _ = DestroyMenu(popup);
                return Err(error);
            }
            popup
        };
        let mut menu = Self {
            hwnd,
            popup,
            original,
            displayed: None,
        };
        menu.refresh();
        Ok(menu)
    }

    pub(super) fn is_popup(&self, popup: usize) -> bool {
        self.popup.0 as usize == popup
    }

    pub(super) fn opened(&self) {
        ACTION.with(|slot| {
            *slot.borrow_mut() = Some(Choice::new(
                Arc::downgrade(&self.original),
                super::input_ticks(),
            ));
        });
    }

    pub(super) fn refresh(&mut self) {
        let report = self.original.report();
        if self.displayed.as_ref() == Some(&report) {
            return;
        }
        let progress = report.status.as_ref().and_then(|status| status.progress);
        let bytes = progress.map_or_else(
            || "No file in progress".to_owned(),
            |progress| {
                format!(
                    "Staged: {} / {} bytes (await publication)",
                    progress.staged_bytes, progress.total_bytes,
                )
            },
        );
        let phase = report.reason().map_or_else(
            || format!("State: {}", report.phase_name().replace('_', " ")),
            |reason| {
                format!(
                    "State: {} ({reason:?})",
                    report.phase_name().replace('_', " "),
                )
            },
        );
        let receipts = report.receipts();
        let published = receipts
            .iter()
            .filter(|receipt| receipt.outcome == "locally_published")
            .count();
        let unconfirmed = receipts
            .iter()
            .filter(|receipt| receipt.durable == Some(false))
            .count();
        let entries = [
            (PHASE, phase),
            (BYTES, bytes),
            (
                RECEIPTS,
                format!("Published: {published} ({unconfirmed} durability unconfirmed)"),
            ),
            (CLEANUP, format!("Receiver cleanup: {}", report.cleanup_name())),
        ];
        let mut updated = true;
        for (id, text) in entries {
            let text = HSTRING::from(text);
            // SAFETY: the live window owns this popup; text is copied now.
            // The receiver's lock was released before any native call.
            updated &= unsafe {
                ModifyMenuW(
                    self.popup,
                    item(id),
                    MF_BYCOMMAND | MF_STRING | MF_GRAYED,
                    id,
                    &text,
                )
            }
            .is_ok();
        }
        let terminal =
            report.cancel_requested || report.reason().is_some() || report.phase_name() == "ended";
        // SAFETY: a local negative-authority action against the original owner.
        unsafe {
            let _ = EnableMenuItem(
                self.popup,
                item(CANCEL),
                MF_BYCOMMAND | if terminal { MF_GRAYED } else { MF_ENABLED },
            );
            let _ = DrawMenuBar(self.hwnd);
        }
        if updated {
            self.displayed = Some(report);
        }
    }
}

impl Drop for Menu {
    fn drop(&mut self) {
        self.original.cancel();
        ACTION.with(|slot| *slot.borrow_mut() = None);
        // The window owns the popup. Cancellation does not assert disk cleanup.
    }
}

const fn item(id: usize) -> u32 {
    assert!(id <= 0xffff);
    #[allow(clippy::cast_possible_truncation)]
    {
        id as u32
    }
}

pub(super) fn document(menu: Option<&Menu>) -> serde_json::Value {
    menu.map_or(serde_json::Value::Null, |menu| {
        let report = menu.original.report();
        let progress = report.status.as_ref().and_then(|status| status.progress);
        let receipts = report
            .receipts()
            .iter()
            .map(|receipt| {
                serde_json::json!({
                    "index": receipt.index,
                    "outcome": receipt.outcome,
                    "bytes": receipt.bytes,
                    "durable": receipt.durable,
                    "reason": receipt.reason,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "requested": true,
            "configured": report.configured,
            "phase": report.phase_name(),
            "cancel_requested": report.cancel_requested,
            "cleanup": report.cleanup_name(),
            "reason": report.reason().map(|reason| format!("{reason:?}")),
            "progress": progress.map(|progress| serde_json::json!({
                "index": progress.id.checked_sub(1),
                "staged_bytes": progress.staged_bytes,
                "total_bytes": progress.total_bytes,
            })),
            "receipts": receipts,
            "results_truncated": report.results_truncated(),
        })
    })
}
