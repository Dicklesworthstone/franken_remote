//! Progress and cancellation for the command's original explicit upload.
//! There is no file picker, retry, or new transfer authority in this menu.
use fr_mobile_ffi::session::{Upload, UploadReport};
use std::{
    cell::RefCell,
    sync::{Arc, Weak},
};
use windows::{
    Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, DestroyMenu, DrawMenuBar, EnableMenuItem, GetMenu, HMENU,
            MF_BYCOMMAND, MF_ENABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, ModifyMenuW,
        },
    },
    core::{HSTRING, Result, w},
};

pub(super) const CANCEL: usize = 0x4680;
const PHASE: usize = 0x4681;
const BYTES: usize = 0x4682;
const RECEIPT: usize = 0x4683;
const CLEANUP: usize = 0x4684;

thread_local! {
    // Cancellation never needs a renderer borrow and can only reach this
    // original selected file. Replacement views have no upload to acquire.
    static ORIGINAL: RefCell<Option<Weak<Upload>>> = const { RefCell::new(None) };
}

pub(super) fn cancel() {
    let original = ORIGINAL.with(|slot| slot.borrow().as_ref().and_then(Weak::upgrade));
    if let Some(original) = original {
        original.cancel();
    }
}

pub(super) struct Menu {
    hwnd: HWND,
    popup: HMENU,
    original: Arc<Upload>,
    displayed: Option<UploadReport>,
}

impl Menu {
    /// The existing window owns the popup only after `AppendMenuW` succeeds.
    /// Every native operation runs after the upload's report lock is released.
    pub(super) fn install(hwnd: HWND, original: Arc<Upload>) -> Result<Self> {
        // SAFETY: only this UI thread's already installed native menu is used.
        let popup = unsafe {
            let menu = GetMenu(hwnd);
            if menu.is_invalid() {
                return Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_FAIL,
                ));
            }
            let popup = CreatePopupMenu()?;
            let attached = AppendMenuW(popup, MF_STRING | MF_GRAYED, PHASE, w!("Selected"))
                .and_then(|()| AppendMenuW(popup, MF_STRING | MF_GRAYED, BYTES, w!("Bytes")))
                .and_then(|()| {
                    AppendMenuW(
                        popup,
                        MF_STRING | MF_GRAYED,
                        RECEIPT,
                        w!("Host receipt: none"),
                    )
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
                .and_then(|()| AppendMenuW(popup, MF_STRING, CANCEL, w!("&Cancel this upload")))
                .and_then(|()| AppendMenuW(menu, MF_POPUP, popup.0 as usize, w!("&File transfer")));
            if let Err(error) = attached {
                let _ = DestroyMenu(popup);
                return Err(error);
            }
            popup
        };
        ORIGINAL.with(|slot| *slot.borrow_mut() = Some(Arc::downgrade(&original)));
        let mut menu = Self {
            hwnd,
            popup,
            original,
            displayed: None,
        };
        menu.refresh();
        Ok(menu)
    }

    pub(super) fn report(&self) -> UploadReport {
        self.original.report()
    }

    pub(super) fn refresh(&mut self) {
        let report = self.report();
        if self.displayed == Some(report) {
            return;
        }
        let progress = report.snapshot.and_then(|snapshot| snapshot.current);
        let bytes = progress.map_or_else(
            || {
                format!(
                    "Selected: {} bytes; no active source",
                    report.selected_bytes
                )
            },
            |progress| {
                format!(
                    "Queued: {} / {} bytes (await host receipt)",
                    progress.queued_bytes,
                    progress.total_bytes.unwrap_or(report.selected_bytes),
                )
            },
        );
        let phase = report.absence.map_or_else(
            || format!("State: {}", report.phase_name().replace('_', " ")),
            |reason| {
                format!(
                    "State: {} ({reason:?})",
                    report.phase_name().replace('_', " ")
                )
            },
        );
        let receipt = report.receipt_detail().map_or_else(
            || report.receipt_name().replace('_', " "),
            |reason| format!("{} ({reason})", report.receipt_name().replace('_', " ")),
        );
        let entries = [
            (PHASE, phase),
            (BYTES, bytes),
            (RECEIPT, format!("Host receipt: {receipt}")),
            (
                CLEANUP,
                format!("Source cleanup: {}", report.cleanup_name()),
            ),
        ];
        let mut updated = true;
        for (id, text) in entries {
            let text = HSTRING::from(text);
            // SAFETY: this popup remains owned by the live viewer window. Text
            // is copied synchronously; no file-state lock spans native calls.
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
        let terminal = report.cancel_requested
            || report.absence.is_some()
            || report.snapshot.is_some_and(|snapshot| {
                snapshot.status.report.is_some_and(|report| report.complete)
            });
        // SAFETY: this changes only the local negative-authority action. A
        // queued Cancel still refers to the same original owner after completion.
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
        ORIGINAL.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot
                .as_ref()
                .is_some_and(|owner| owner.ptr_eq(&Arc::downgrade(&self.original)))
            {
                *slot = None;
            }
        });
        // The native window owns and destroys the popup. Dropping this view
        // does not erase receipts or detach the original session's disk owner.
    }
}

const fn item(id: usize) -> u32 {
    assert!(id <= 0xffff);
    #[allow(clippy::cast_possible_truncation)]
    {
        id as u32
    }
}

/// One content-free snapshot. Only an actual host receipt describes publication.
pub(super) fn document(report: Option<&UploadReport>) -> serde_json::Value {
    report.map_or(serde_json::Value::Null, |report| {
        let progress = report.snapshot.and_then(|snapshot| snapshot.current);
        serde_json::json!({
            "selected_bytes": report.selected_bytes,
            "configured": report.configured,
            "phase": report.phase_name(),
            "cancel_requested": report.cancel_requested,
            "queued_bytes": progress.map(|progress| progress.queued_bytes),
            "total_bytes": progress.and_then(|progress| progress.total_bytes),
            "receipt": report.receipt_name(),
            "receipt_detail": report.receipt_detail(),
            "published_bytes": report.published_bytes(),
            "cleanup": report.cleanup_name(),
            "reason": report.absence.map(|reason| format!("{reason:?}")),
            "succeeded": report.is_success(),
        })
    })
}
