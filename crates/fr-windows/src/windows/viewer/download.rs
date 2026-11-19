//! Receive status and file-only cancellation for one original local destination.
//! The session owns the disk worker. This menu never opens paths or grants it
//! authority, and remains attached to its original receipts across view changes.
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
const PROGRESS: usize = 0x4692;
const RECEIPTS: usize = 0x4693;
const CLEANUP: usize = 0x4694;

thread_local! {
    // A nested callback can stop only the destination named by its original
    // popup, without waiting for a borrow of the native viewer.
    static ACTION: RefCell<Option<Choice<Weak<Download>>>> = const { RefCell::new(None) };
}

pub(super) fn cancel() {
    // SAFETY: the current command's original timestamp on this UI thread.
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
        // SAFETY: this UI thread's viewer owns the parent menu. The new popup
        // is destroyed on failure or transferred to that window on success.
        let popup = unsafe {
            let menu = GetMenu(hwnd);
            if menu.is_invalid() {
                return Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_FAIL,
                ));
            }
            let popup = CreatePopupMenu()?;
            let attached = AppendMenuW(popup, MF_STRING | MF_GRAYED, PHASE, w!("Receive state"))
                .and_then(|()| {
                    AppendMenuW(popup, MF_STRING | MF_GRAYED, PROGRESS, w!("Staged bytes"))
                })
                .and_then(|()| {
                    AppendMenuW(popup, MF_STRING | MF_GRAYED, RECEIPTS, w!("Local receipts"))
                })
                .and_then(|()| {
                    AppendMenuW(popup, MF_STRING | MF_GRAYED, CLEANUP, w!("Disk cleanup"))
                })
                .and_then(|()| AppendMenuW(popup, MF_SEPARATOR, 0, w!("")))
                .and_then(|()| AppendMenuW(popup, MF_STRING, CANCEL, w!("Stop &receiving files")))
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

    pub(super) fn opened(&mut self) {
        ACTION.with(|slot| {
            *slot.borrow_mut() = Some(Choice::new(
                Arc::downgrade(&self.original),
                super::input_ticks(),
            ));
        });
        self.displayed = None;
        self.refresh();
    }

    pub(super) fn succeeded(&self) -> bool {
        self.original.report().is_success()
    }

    pub(super) fn cleanup_complete(&self) -> bool {
        self.original.report().cleanup_complete()
    }

    pub(super) fn refresh(&mut self) {
        let report = self.original.report();
        if self.displayed.as_ref() == Some(&report) {
            return;
        }
        let phase = report.reason_code().map_or_else(
            || format!("State: {}", report.phase_name().replace('_', " ")),
            |reason| {
                format!(
                    "State: {} ({reason})",
                    report.phase_name().replace('_', " ")
                )
            },
        );
        let progress = report
            .status
            .as_ref()
            .and_then(|status| status.progress)
            .map_or_else(
                || "No file currently staging".to_owned(),
                |progress| {
                    format!(
                        "Staged: {} / {} bytes (not yet published)",
                        progress.staged_bytes, progress.total_bytes,
                    )
                },
            );
        let receipts = report.receipts();
        let published = receipts
            .iter()
            .filter(|receipt| receipt.bytes.is_some())
            .count();
        let unconfirmed = receipts
            .iter()
            .filter(|receipt| receipt.durable == Some(false))
            .count();
        let mut receipt_summary = format!(
            "Published: {published} ({unconfirmed} durability unconfirmed); other outcomes: {}",
            receipts.len() - published,
        );
        if report.results_truncated() {
            receipt_summary.push_str("; result limit reached");
        }
        let entries = [
            (PHASE, phase),
            (PROGRESS, progress),
            (RECEIPTS, receipt_summary),
            (CLEANUP, format!("Disk cleanup: {}", report.cleanup_name())),
        ];
        let mut updated = true;
        for (id, text) in entries {
            let text = HSTRING::from(text);
            // SAFETY: window-owned popup; Win32 copies the bounded text before
            // return. No receive-state lock spans a native call.
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
        // SAFETY: local menu state only. Even a previously queued command can
        // only cancel its original receiver, never a replacement connection.
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
        if self.original.report().phase_name() != "ended" {
            self.original.cancel();
        }
        ACTION.with(|slot| *slot.borrow_mut() = None);
        // The native window owns the popup; the original session owns cleanup.
        // A completed owner's retained evidence is not changed by menu teardown.
    }
}

fn item(id: usize) -> u32 {
    u32::try_from(id).expect("fixed native receive menu identifier")
}

pub(super) fn document(menu: Option<&Menu>) -> serde_json::Value {
    menu.map_or(serde_json::Value::Null, |menu| {
        let report = menu.original.report();
        let progress = report.status.as_ref().and_then(|status| status.progress);
        let receipts: Vec<_> = report
            .receipts()
            .iter()
            .map(|receipt| {
                serde_json::json!({
                    "index": receipt.index,
                    "outcome": receipt.outcome,
                    "bytes": receipt.bytes,
                    "id": receipt.id,
                    "receipt": receipt.receipt,
                    "published_bytes": receipt.bytes,
                    "durable": receipt.durable,
                    "reason": receipt.reason,
                })
            })
            .collect();
        serde_json::json!({
            "requested": true,
            "configured": report.configured,
            "phase": report.phase_name(),
            "cancel_requested": report.cancel_requested,
            "progress": progress.map(|progress| serde_json::json!({
                "index": progress.id.checked_sub(1),
                "id": progress.id,
                "staged_bytes": progress.staged_bytes,
                "total_bytes": progress.total_bytes,
            })),
            "receipts": receipts,
            "results_truncated": report.results_truncated(),
            "cleanup": report.cleanup_name(),
            "reason": report.reason().map(|reason| format!("{reason:?}")),
            "reason_code": report.reason_code(),
            "succeeded": report.is_success(),
        })
    })
}
