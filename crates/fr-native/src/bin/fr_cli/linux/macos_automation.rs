//! Connect the original Mac foreground session to the existing authenticated
//! local action ledger. No new input semantics, queue, lease or wire protocol.
use super::Failure;
use crate::{
    options::{Connection, DisplayChoice},
    session_control::input::Shared,
};
use fr_client::robot::live::Evidence;
use fr_native::macos::session::Automation;
use frd::{
    media::PresentationStage,
    native_connection::reconnect::CallbackError,
    session_startup::{ControlledViewer, Presentation},
};
use std::time::Instant;

/// Repeat the public command's consent boundary before any local selection,
/// device preparation, owner socket or remote lookup can begin.
pub(super) fn check(connection: &Connection) -> Result<(), Failure> {
    if !connection.control
        || connection.attempts != 1
        || connection.choose_host
        || connection.pick.is_some()
        || connection.display == DisplayChoice::Choose
        || connection.clipboard
        || connection.capture_system_shortcuts
        || connection.audio.is_some()
        || connection.microphone.is_some()
        || connection.receive.is_some()
        || connection.choose_receive_directory
        || !connection.upload.paths.is_empty()
        || connection.upload.file_drop
        || connection.upload.file_commands
        || connection.upload.choose_files
    {
        return Err(Failure::new(
            "robot_input_profile_invalid",
            "Foreground robot control requires one explicit machine and display, one attempt, and no clipboard, audio, microphone, files or interactive selection.",
            2,
        ));
    }
    Ok(())
}

pub(super) struct Original(pub(super) Shared);

fn evidence(frame: Presentation) -> Evidence {
    match frame.stage {
        PresentationStage::DecodedOnly => Evidence::Decoded,
        PresentationStage::SubmittedToCompositor => Evidence::SubmittedToCompositor,
    }
}

impl Automation for Original {
    fn ready(
        &self,
        snapshot: fr_client::robot::snapshot::Handle,
        display: fr_wire::display::Display,
        initial: Presentation,
        received_at: Instant,
    ) -> Result<(), CallbackError> {
        self.0
            .bind_snapshot_at(
                snapshot,
                display,
                initial.frame.as_raw(),
                evidence(initial),
                received_at,
            )
            .map_err(|()| CallbackError)
    }

    fn presented(&self, frame: Presentation) -> Result<(), CallbackError> {
        self.0
            .presented(frame.frame.as_raw(), evidence(frame))
            .map_err(|()| CallbackError)
    }

    fn turn(&self, viewer: &mut ControlledViewer) {
        // The session has just checked native focus, visibility, geometry and
        // its independent resize fence on this same synchronous turn.
        self.0.turn(viewer, true);
    }

    fn result(&self, result: fr_client::input::ResultEvent) {
        self.0.result(result);
    }

    fn retire(&self) {
        self.0.retire();
    }
}
