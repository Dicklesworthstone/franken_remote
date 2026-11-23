//! Explicit local download consent on the original Linux desktop.
//! Receipt formatting is shared with other native clients.
pub(in crate::linux) use crate::file_reports::download::Summary;
use crate::file_reports::download::absence_code;
use crate::{Failure, options::ReceiveOptions};
use fr_native::desktop::{Desktop, Error as DesktopError};
use frd::native_files::{ReceiveControl, ReceiveRequest};

pub(in crate::linux) struct Local {
    request: Option<ReceiveRequest>,
    attempt: Option<Attempt>,
}
enum Attempt {
    Configured(ReceiveControl),
    Absent(&'static str),
}

pub(in crate::linux) fn select(options: Option<&ReceiveOptions>) -> Result<Option<Local>, Failure> {
    let Some(options) = options else {
        return Ok(None);
    };
    let directory = options.open()?;
    let request = options.request(&directory)?;
    Ok(Some(Local {
        request: Some(request),
        attempt: None,
    }))
}

impl Local {
    pub(in crate::linux) fn folder_sync_available(&self) -> bool {
        self.request
            .as_ref()
            .is_some_and(|request| request.sync_job_id().is_some())
    }

    pub(in crate::linux) fn configure(&mut self, desktop: &mut Desktop) {
        // Explicit consent and any outcome belong to the first opened attempt.
        // A later callback must not replace its evidence or reapprove receipt.
        if self.attempt.is_some() {
            return;
        }
        let Some(request) = self.request.take() else {
            self.attempt = Some(Attempt::Absent("files_receive_not_started"));
            return;
        };
        self.attempt = Some(match desktop.request_receive_files(request) {
            Ok(control) => Attempt::Configured(control),
            Err(DesktopError::Files(absence)) => Attempt::Absent(absence_code(absence)),
            Err(_) => Attempt::Absent("files_receive_unavailable"),
        });
    }

    pub(in crate::linux) fn summary(&self) -> Summary {
        match &self.attempt {
            Some(Attempt::Configured(control)) => Summary::from_status(&control.status()),
            Some(Attempt::Absent(code)) => Summary::unavailable(code),
            None => Summary::unavailable("files_receive_not_started"),
        }
    }
}
