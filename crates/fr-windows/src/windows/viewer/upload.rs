//! Explicit native file choices on one original controlled upload lane.
//! Every choice pins one descriptor and consumes its own bounded batch result.
#[path = "file_picker.rs"]
mod file_picker;
use crate::{change_display, control::Choice};
use fr_mobile_ffi::session::{FileChoiceContext, Shared, Upload, UploadReport};
use std::{
    cell::RefCell,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
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

pub(super) const CANCEL: usize = 0x4680;
pub(super) const OPEN: usize = 0x4685;
pub(super) const PAUSE: usize = 0x4686;
pub(super) const RESUME: usize = 0x4687;
const PHASE: usize = 0x4681;
const BYTES: usize = 0x4682;
const RECEIPT: usize = 0x4683;
const CLEANUP: usize = 0x4684;
type BatchChoice = Choice<(Weak<Upload>, u8)>;

thread_local! {
    // Each popup captures the exact batch it displayed; late Cancel/pacing
    // commands can never affect a subsequently selected file.
    static ACTION: RefCell<Option<BatchChoice>> = const { RefCell::new(None) };
    static PICKER_STOP: RefCell<Option<Weak<AtomicBool>>> = const { RefCell::new(None) };
    // Keep a completed worker's negative notification until UI synchronization;
    // collecting Choosing must not discard an external foreground-loss signal.
    static PICKER_FOCUS: RefCell<Option<Arc<file_picker::Focus>>> = const { RefCell::new(None) };
}

fn action(change: impl FnOnce(&Upload, u8)) {
    // SAFETY: original queued command timestamp on this UI thread.
    let message = unsafe { GetMessageTime() }.cast_unsigned();
    let selected = ACTION.with(|slot| {
        slot.borrow_mut()
            .as_mut()?
            .take(message, file_picker::ticks())
    });
    if let Some(((owner, batch), _age)) = selected
        && let Some(owner) = owner.upgrade()
    {
        change(&owner, batch);
    }
}

pub(super) fn cancel() {
    action(|owner, batch| {
        owner.cancel_batch(batch);
    });
}
pub(super) fn pause() {
    action(|owner, batch| {
        owner.pace_batch(batch, true);
    });
}
pub(super) fn resume() {
    action(|owner, batch| {
        owner.pace_batch(batch, false);
    });
}

/// A lifecycle fence may run reentrantly while Viewer/Menu is borrowed.
/// It can only stop this original local choice, never submit selected content.
pub(super) fn retire_choice() {
    PICKER_STOP.with(|slot| {
        if let Some(stop) = slot.borrow().as_ref().and_then(Weak::upgrade) {
            stop.store(true, Ordering::SeqCst);
        }
    });
}

/// Application activation is per input thread, including threads in this same
/// process. Only the published original chooser may receive this handoff.
pub(super) fn picker_deactivated(thread: u32, lifetime: u64) -> bool {
    PICKER_FOCUS.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|focus| focus.allows_picker(thread, lifetime))
    })
}

/// Scoped worker-side hiding has already fenced its exact original grant.
/// The UI consumes this only for its matching native lifetime and attempt.
pub(super) fn picker_focus_lost(lifetime: u64) -> bool {
    PICKER_FOCUS.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|focus| focus.take_hidden(lifetime))
    })
}

struct Choosing {
    original: Arc<Shared>,
    context: FileChoiceContext,
    attempt: u64,
    lifetime: u64,
    picker: file_picker::Picker,
    stop: Arc<AtomicBool>,
}

pub(super) struct Menu {
    hwnd: HWND,
    popup: HMENU,
    original: Arc<Upload>,
    displayed: Option<UploadReport>,
    choice: Option<change_display::MenuChoice<Shared>>,
    choosing: Option<Choosing>,
    available: bool,
    last_choice: &'static str,
    cleanup_failed: bool,
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
            let attached = AppendMenuW(
                popup,
                MF_STRING | MF_GRAYED,
                OPEN,
                w!("&Pick and send one file…"),
            )
            .and_then(|()| AppendMenuW(popup, MF_SEPARATOR, 0, w!("")))
            .and_then(|()| AppendMenuW(popup, MF_STRING | MF_GRAYED, PHASE, w!("No file selected")))
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
            .and_then(|()| AppendMenuW(popup, MF_STRING, PAUSE, w!("&Pause this upload")))
            .and_then(|()| AppendMenuW(popup, MF_STRING, RESUME, w!("&Resume this upload")))
            .and_then(|()| AppendMenuW(menu, MF_POPUP, popup.0 as usize, w!("&File transfer")));
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
            choice: None,
            choosing: None,
            available: false,
            last_choice: "none",
            cleanup_failed: false,
        };
        menu.refresh();
        Ok(menu)
    }

    pub(super) fn is_popup(&self, popup: usize) -> bool {
        self.popup.0 as usize == popup
    }

    pub(super) fn opened(&mut self, shared: &Arc<Shared>, lifetime: u64, available: bool) {
        let attempt = shared.frames.app_id(0);
        let opened = file_picker::ticks();
        self.available =
            available && !self.cleanup_failed && !self.choosing() && self.original.ready();
        self.choice = self.available.then(|| {
            change_display::MenuChoice::new(Arc::clone(shared), lifetime, attempt, opened)
        });
        let batch = self.report().batch;
        ACTION.with(|slot| {
            *slot.borrow_mut() = Some(Choice::new((Arc::downgrade(&self.original), batch), opened));
        });
        self.displayed = None;
        self.refresh();
    }

    pub(super) fn open_choice(
        &mut self,
        shared: &Arc<Shared>,
        lifetime: u64,
        message: u32,
        available: bool,
    ) -> bool {
        let Some(mut choice) = self.choice.take() else {
            return false;
        };
        let Some(request) = choice.take(
            shared,
            lifetime,
            shared.frames.app_id(0),
            available && !self.cleanup_failed && !self.choosing() && self.original.ready(),
            message,
            file_picker::ticks(),
        ) else {
            return false;
        };
        let Ok(context) = shared.begin_file_choice() else {
            return false;
        };
        let Ok(picker) = file_picker::Picker::open(
            request.at,
            Arc::clone(&request.original),
            context.clone(),
            self.hwnd,
            request.lifetime,
        ) else {
            self.last_choice = "native_unavailable";
            return false;
        };
        let stop = picker.cancellation();
        PICKER_STOP.with(|slot| *slot.borrow_mut() = Some(Arc::downgrade(&stop)));
        PICKER_FOCUS.with(|slot| *slot.borrow_mut() = Some(picker.focus()));
        self.choosing = Some(Choosing {
            original: request.original,
            context,
            attempt: request.attempt,
            lifetime: request.lifetime,
            picker,
            stop,
        });
        self.last_choice = "choosing";
        self.available = false;
        self.displayed = None;
        if let Some(choosing) = &mut self.choosing {
            choosing.picker.start();
        }
        true
    }

    pub(super) fn choosing(&self) -> bool {
        self.choosing.is_some()
    }

    pub(super) fn retire_choice(&mut self) {
        self.choice = None;
        if let Some(choosing) = &self.choosing {
            choosing.picker.cancel();
        }
        self.available = false;
        self.displayed = None;
    }

    pub(super) fn poll(&mut self, current: &Arc<Shared>, lifetime: u64, available: bool) {
        if let Some(choosing) = &mut self.choosing {
            let ready = current.file_choice_ready(&choosing.context);
            let same = Arc::ptr_eq(&choosing.original, current)
                && choosing.attempt == current.frames.app_id(0)
                && choosing.lifetime == lifetime;
            if !same || !available || ready.is_err() || !self.original.ready() {
                choosing.picker.cancel();
            }
            if let Some(result) = choosing.picker.poll() {
                let result = result.and_then(|selected| {
                    if !same || !available || !ready.is_ok_and(|ready| ready) {
                        return Err(crate::file_choice::Error::Cancelled);
                    }
                    selected.submit(&self.original, &choosing.stop, || {
                        current
                            .file_choice_ready(&choosing.context)
                            .is_ok_and(|ready| ready)
                    })
                });
                self.last_choice = match result {
                    Ok(()) => "selected_once",
                    Err(crate::file_choice::Error::Cancelled) => "cancelled",
                    Err(crate::file_choice::Error::Expired | crate::file_choice::Error::Clock) => {
                        "expired"
                    }
                    Err(crate::file_choice::Error::Cleanup) => "cleanup_failed",
                    Err(_) => "selection_refused",
                };
                self.cleanup_failed |= matches!(result, Err(crate::file_choice::Error::Cleanup));
                if let Err(error) = result
                    && error != crate::file_choice::Error::Cancelled
                {
                    eprintln!("fr-windows: file_choice_refused ({error:?})");
                }
                self.choosing = None;
                PICKER_STOP.with(|slot| *slot.borrow_mut() = None);
                self.displayed = None;
            }
        }
        let available =
            available && !self.cleanup_failed && !self.choosing() && self.original.ready();
        if self.available != available {
            self.displayed = None;
        }
        self.available = available;
    }

    pub(super) fn cleanup_complete(&self) -> bool {
        self.choosing.is_none() && !self.cleanup_failed
    }

    pub(super) fn fail_cleanup(&mut self) {
        self.cleanup_failed = true;
        self.retire_choice();
    }

    pub(super) fn succeeded(&self) -> bool {
        self.cleanup_complete() && !self.cleanup_failed && self.original.is_success()
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
        let terminal = report.batch == 0
            || report.cancel_requested
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
            let snapshot = report.snapshot;
            for (id, enabled) in [
                (OPEN, self.available),
                (
                    PAUSE,
                    !terminal && snapshot.is_some_and(|s| !s.pause_requested),
                ),
                (
                    RESUME,
                    !terminal && snapshot.is_some_and(|s| s.pause_requested),
                ),
            ] {
                let _ = EnableMenuItem(
                    self.popup,
                    item(id),
                    MF_BYCOMMAND | if enabled { MF_ENABLED } else { MF_GRAYED },
                );
            }
            let _ = DrawMenuBar(self.hwnd);
        }
        if updated {
            self.displayed = Some(report);
        }
    }
}

impl Drop for Menu {
    fn drop(&mut self) {
        self.retire_choice();
        if self.choosing.is_some() {
            self.cleanup_failed = true;
        }
        PICKER_STOP.with(|slot| *slot.borrow_mut() = None);
        PICKER_FOCUS.with(|slot| *slot.borrow_mut() = None);
        ACTION.with(|slot| *slot.borrow_mut() = None);
        // Native window owns the popup. A stuck cancelled picker worker is
        // retained separately until its actual join; never called clean here.
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
pub(super) fn document(menu: Option<&Menu>) -> serde_json::Value {
    menu.map_or(serde_json::Value::Null, |menu| {
        let mut value = report_document(&menu.report());
        value["batches"] = serde_json::json!(
            menu.original
                .reports()
                .iter()
                .map(report_document)
                .collect::<Vec<_>>()
        );
        value["choice"] = serde_json::json!(menu.last_choice);
        value["choice_cleanup"] = serde_json::json!(if menu.cleanup_failed {
            "failed"
        } else if menu.cleanup_complete() {
            "finished"
        } else {
            "pending"
        });
        value["succeeded"] = serde_json::json!(menu.succeeded());
        value
    })
}

fn report_document(report: &UploadReport) -> serde_json::Value {
    let progress = report.snapshot.and_then(|snapshot| snapshot.current);
    serde_json::json!({
        "batch": report.batch,
        "requested": report.batch != 0,
        "selected_bytes": report.selected_bytes,
        "configured": report.configured,
        "phase": report.phase_name(),
        "cancel_requested": report.cancel_requested,
        "pause_requested": report.snapshot.map(|s| s.pause_requested),
        "paused": report.snapshot.and_then(|s| s.paused),
        "queued_bytes": progress.map(|progress| progress.queued_bytes),
        "total_bytes": progress.and_then(|progress| progress.total_bytes),
        "receipt": report.receipt_name(),
        "receipt_detail": report.receipt_detail(),
        "published_bytes": report.published_bytes(),
        "cleanup": report.cleanup_name(),
        "reason": report.absence.map(|reason| format!("{reason:?}")),
        "succeeded": report.is_success(),
    })
}
