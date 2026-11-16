//! Explicit local iOS selection over the original native ATP upload owner.
//! Choosing/pinning a descriptor grants no authority. Only a new Send gesture,
//! after returning to a current controlled view, can admit its contents.
#![forbid(unsafe_code)]

use super::{ControlState, InputResult, PreparedUpload, Shared, Upload, control_request};
use crate::upload_selection::{Error as ChoiceError, Phase};
use frd::session_startup::FileSendError;
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

struct Preparing<'a>(&'a AtomicBool);

impl Drop for Preparing<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    Unavailable,
    Closed,
    Input(InputResult),
    Choice(ChoiceError),
    Source,
    Transfer(FileSendError),
}

impl Shared {
    fn upload_live(&self) -> bool {
        self.serving.load(Ordering::SeqCst)
            && !self.close.load(Ordering::SeqCst)
            && self.local_stop.load(Ordering::SeqCst) == 0
            && !self.finished()
    }

    pub(crate) fn clear_upload_selection(&self) {
        self.upload_selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// The native picker may cover an observing view, never a granted or
    /// requested controller. It retains only one original-attempt choice.
    pub(crate) fn begin_upload_selection(&self) -> Result<u64, Error> {
        let attempt = self.frames.app_id(0);
        self.frames
            .with_current(attempt, || {
                if !self.upload_live() {
                    return Err(Error::Closed);
                }
                if self.upload_preparing.load(Ordering::SeqCst) {
                    return Err(Error::Input(InputResult::Busy));
                }
                if !self.visible.load(Ordering::SeqCst)
                    || !self.active.load(Ordering::SeqCst)
                    || self.control_state() != ControlState::Viewing
                {
                    return Err(Error::Input(InputResult::NotControlled));
                }
                if !self.upload.as_ref().is_some_and(|upload| upload.can_select()) {
                    return Err(Error::Unavailable);
                }
                self.upload_selection
                    .lock()
                    .map_err(|_| Error::Closed)?
                    .begin(attempt, Instant::now())
                    .map_err(Error::Choice)
            })
            .map_err(|_| Error::Choice(ChoiceError::Stale))?
    }

    /// Called off the UI and session threads while the caller retains the
    /// selected URL's security-scoped access. No file bytes are read here.
    /// The original attempt, token and deadline are rechecked after disk I/O.
    pub(crate) fn prepare_upload(&self, token: u64, path: &Path) -> Result<(), Error> {
        // Cancelled native providers may still be inside a blocking open. A
        // new choice cannot multiply those workers while the old call returns.
        self.upload_preparing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| Error::Input(InputResult::Busy))?;
        let _preparing = Preparing(&self.upload_preparing);
        let attempt = self.frames.app_id(0);
        self.frames
            .with_current(attempt, || {
                if !self.upload_live() {
                    return Err(Error::Closed);
                }
                self.upload_selection
                    .lock()
                    .map_err(|_| Error::Closed)?
                    .begin_prepare(token, attempt, Instant::now())
                    .map_err(Error::Choice)
            })
            .map_err(|_| Error::Choice(ChoiceError::Stale))??;
        let prepared = match Upload::prepare(path) {
            Ok(prepared) => prepared,
            Err(_) => {
                self.discard_upload_selection(token);
                return Err(Error::Source);
            }
        };
        self.frames
            .with_current(attempt, || {
                if !self.upload_live() {
                    return Err(Error::Closed);
                }
                self.upload_selection
                    .lock()
                    .map_err(|_| Error::Closed)?
                    .finish_prepare(token, attempt, Instant::now(), prepared)
                    .map_err(Error::Choice)
            })
            .map_err(|_| Error::Choice(ChoiceError::Stale))?
    }

    /// A delayed picker dismissal can retire only the choice it displayed.
    pub(crate) fn discard_upload_selection(&self, token: u64) -> bool {
        self.upload_selection
            .lock()
            .is_ok_and(|mut selections| selections.discard(token))
    }

    /// Consume exactly one prepared selection on an explicit current Send.
    /// Every result spends it: the shell must never retry a refused gesture.
    pub(crate) fn send_upload(&self, token: u64) -> Result<(), Error> {
        let began = Instant::now();
        let attempt = self.frames.app_id(0);
        let prepared = self
            .upload_selection
            .lock()
            .map_err(|_| Error::Closed)?
            .take(token, attempt, Instant::now())
            .map_err(Error::Choice)?;
        if !self.upload_live() {
            return Err(Error::Closed);
        }
        let upload = self.upload.as_ref().ok_or(Error::Unavailable)?;
        let context = self.begin_file_choice().map_err(Error::Input)?;
        let captured = self
            .capture_input()
            .map_err(Error::Input)?
            .with_queue_age(began.elapsed())
            .ok_or(Error::Input(InputResult::Busy))?;
        upload
            .submit_if(prepared, || {
                if began.elapsed() >= control_request::MAX_GESTURE_AGE
                    || self.frames.app_id(0) != attempt
                {
                    return false;
                }
                self.with_granted(|granted| {
                    if self.visible.load(Ordering::SeqCst)
                        && self.upload_live()
                        && captured.current(granted)
                        && std::sync::Arc::ptr_eq(&context.stop, &granted.stop)
                        && granted
                            .source
                            .input_context_ready(&context.input)
                            .unwrap_or(false)
                        && began.elapsed() < control_request::MAX_GESTURE_AGE
                        && self.visible.load(Ordering::SeqCst)
                        && self.upload_live()
                    {
                        InputResult::Sent
                    } else {
                        // Zero admission is a local refusal, not a reason to
                        // stop a successor's grant or retry this file.
                        InputResult::Busy
                    }
                }) == InputResult::Sent
            })
            .map_err(Error::Transfer)
    }

    pub(crate) fn cancel_upload_batch(&self, batch: u8) -> bool {
        self.upload.as_ref().is_some_and(|upload| upload.cancel_batch(batch))
    }

    pub(crate) fn pause_upload_batch(&self, batch: u8, paused: bool) -> bool {
        self.upload
            .as_ref()
            .is_some_and(|upload| upload.pace_batch(batch, paused))
    }

    /// Real source collection, including collection after an original cleanup
    /// timeout. This never changes the transfer's failure or permits replay.
    pub(crate) fn upload_sources_released(&self) -> bool {
        self.upload_sources_released.load(Ordering::SeqCst)
            && !self.upload_preparing.load(Ordering::SeqCst)
    }

    /// Versioned, bounded and content-free. Selected filenames/URLs remain in
    /// the local native picker; none enter diagnostics or this JSON document.
    pub(crate) fn upload_document(&self) -> serde_json::Value {
        let attempt = self.frames.app_id(0);
        let selection = self.upload_selection.lock().ok().and_then(|mut selections| {
            let now = Instant::now();
            let status = selections.status(attempt, now).ok().flatten()?;
            let bytes = selections
                .prepared(status.token, attempt, now)
                .ok()
                .map(PreparedUpload::selected_bytes);
            Some(serde_json::json!({
                "token": status.token,
                "attempt": status.attempt,
                "state": match status.phase {
                    Phase::AwaitingPreparation => "choosing",
                    Phase::Preparing => "preparing",
                    Phase::Ready => "prepared",
                },
                "selected_bytes": bytes,
            }))
        });
        let upload = self.upload.as_ref();
        serde_json::json!({
            "version": 1,
            "available": upload.is_some_and(|upload| upload.can_select()),
            "ready": upload.is_some_and(|upload| upload.ready()),
            "sources_released": self.upload_sources_released(),
            "selection": selection.unwrap_or_else(|| serde_json::json!({
                "token": 0,
                "attempt": 0,
                "state": "none",
                "selected_bytes": null,
            })),
            "transfer": upload.map(|upload| upload.report().document()),
            "history": upload.map_or_else(Vec::new, |upload| {
                upload.reports().iter().map(super::files::Report::document).collect()
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unconfigured_session_cannot_choose_prepare_or_send_a_file() {
        let shared = Shared::with_auxiliary(true, false, Some(Upload::interactive()), None);
        assert!(shared.begin_upload_selection().is_err());
        assert!(shared.prepare_upload(1, Path::new("/not-opened")).is_err());
        assert!(shared.send_upload(1).is_err());
        assert!(!shared.discard_upload_selection(1));
        assert!(!shared.cancel_upload_batch(1));
        assert!(!shared.pause_upload_batch(1, true));
        assert_eq!(shared.upload_document()["selection"]["token"], 0);
        assert_eq!(shared.upload_document()["history"], serde_json::json!([]));
    }

    #[test]
    fn terminal_reporting_does_not_claim_original_sources_were_collected() {
        let shared = Shared::with_auxiliary(true, false, Some(Upload::interactive()), None);
        assert!(!shared.upload_sources_released());
        super::super::finish(&shared, super::super::end::INTERNAL, None);
        assert!(shared.finished());
        assert!(!shared.upload_sources_released());
        assert_eq!(shared.upload_document()["sources_released"], false);
        assert_eq!(shared.upload_document()["selection"]["token"], 0);
        assert!(!shared.upload.as_ref().unwrap().available());
    }

    #[test]
    fn an_outstanding_preparation_blocks_another_and_delays_release_evidence() {
        let shared = Shared::new();
        shared.upload_preparing.store(true, Ordering::SeqCst);
        assert!(!shared.upload_sources_released());
        assert_eq!(
            shared.prepare_upload(1, Path::new("/not-opened")),
            Err(Error::Input(InputResult::Busy)),
        );
        shared.clear_upload_selection();
        assert!(shared.upload_preparing.load(Ordering::SeqCst));
        let preparing = Preparing(&shared.upload_preparing);
        drop(preparing);
        assert!(shared.upload_sources_released());
    }

    #[test]
    fn sessions_without_uploads_need_no_external_source_permission() {
        let shared = Shared::new();
        assert!(shared.upload_sources_released());
        assert_eq!(shared.upload_document()["transfer"], serde_json::Value::Null);
        assert_eq!(shared.upload_document()["available"], false);
    }
}
