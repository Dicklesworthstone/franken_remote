//! Completion evidence for the native mobile and Windows session owner.
//! Local cancellation is a user-visible end, but only successful service and
//! cleanup permit a window to replace its session after a resize.
use frd::{
    native_connection::reconnect::Failure,
    session_startup::{ControlledViewerError, Error, ObserverError, StreamingViewerError},
};

/// Why a session ended (`FR_END_*` in `include/fr_mobile.h`). Content-free.
pub mod end {
    pub const OK: u32 = 0;
    pub const HOST_CLOSED: u32 = 1;
    pub const NAME_UNRESOLVED: u32 = 2;
    pub const OUTSIDE_TAILNET: u32 = 3;
    pub const IDENTITY_CHANGED: u32 = 4;
    pub const TLS_REFUSED: u32 = 5;
    pub const UNREACHABLE: u32 = 6;
    pub const HOST_REFUSED: u32 = 7;
    pub const SESSION_FAILED: u32 = 8;
    pub const CANCELLED: u32 = 9;
    pub const INTERNAL: u32 = 10;
    /// Control stopped on this device (the view left the screen, input was
    /// refused, or the picture changed size), and the session with it.
    pub const CONTROL_STOPPED: u32 = 11;
    /// The host is serving another viewer (one at a time).
    pub const HOST_BUSY: u32 = 12;
}

/// Why this device stopped control (`CONTROL_STOPPED`'s detail). The first
/// reason recorded wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(super) enum LocalStop {
    Hidden = 1,
    InputRefused = 2,
    PictureChanged = 3,
}
impl LocalStop {
    pub(super) const fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::Hidden),
            2 => Some(Self::InputRefused),
            3 => Some(Self::PictureChanged),
            _ => None,
        }
    }
}

/// Proof used only to reopen viewing after a local window resize. The
/// supervisor returns success after the original owner's bounded cleanup;
/// cancellation classified as `OK` does not establish that success.
pub(super) fn orderly_closed(
    result: Result<(), Failure>,
    service_succeeded: bool,
    closed: bool,
    stopped: Option<LocalStop>,
    terminal: Option<ObserverError>,
) -> bool {
    result.is_ok() && service_succeeded && closed && stopped.is_none() && terminal.is_none()
}

/// The end code and detail for a finished run. A local close or stop cannot
/// erase an authenticated terminal report, including its external effects.
pub(super) fn ending(
    result: Result<(), Failure>,
    closed: bool,
    stopped: Option<LocalStop>,
    terminal: Option<ObserverError>,
) -> (u32, Option<String>) {
    let terminal = terminal.or_else(|| match result {
        Err(Failure::Observation(error))
            if terminal_report(error) && !requested_terminal(error) =>
        {
            Some(error)
        }
        _ => None,
    });
    if let Some(error) = terminal {
        let reported = Failure::Observation(error);
        // Cleanup failure remains primary, and the original owner's report
        // remains visible alongside it. Neither proves a safe replacement.
        return if let Err(failure @ (Failure::Cleanup | Failure::CleanupExpired)) = result {
            (
                classify(failure, false),
                Some(format!("{failure:?}; terminal: {reported:?}")),
            )
        } else {
            (classify(reported, false), Some(format!("{reported:?}")))
        };
    }
    match (result, stopped) {
        // The first local control stop stays visible when a later window
        // close races it. Asking to close does not undo the earlier failure.
        (Ok(()) | Err(Failure::Observation(_)), Some(reason)) => {
            (end::CONTROL_STOPPED, Some(format!("{reason:?}")))
        }
        (Ok(()), None) if closed => (end::OK, None),
        (Ok(()), None) => (end::HOST_CLOSED, None),
        (Err(failure), _) => (classify(failure, closed), Some(format!("{failure:?}"))),
    }
}

fn terminal_report(error: ObserverError) -> bool {
    matches!(
        error,
        ObserverError::Streaming(StreamingViewerError::Control(
            ControlledViewerError::LeaseRevoked(_)
        )) | ObserverError::Streaming(StreamingViewerError::Session(Error::RemoteClosed(_)))
    )
}

/// Content-free classification for the app's message. The exact typed
/// failure stays available as `fr_session_copy_detail`.
pub(super) fn classify(failure: Failure, closed: bool) -> u32 {
    use frd::native_connection::Error as C;
    // A locally cancelled dial or reconnect can finish normally without
    // proving that a serving session closed. A failed observer is never
    // converted to success solely because the user also asked to close.
    if closed
        && (matches!(
            failure,
            Failure::Cancelled | Failure::Notification | Failure::Connection(C::Cancelled)
        ) || matches!(failure, Failure::Observation(error) if requested_terminal(error)))
    {
        return end::OK;
    }
    match failure {
        Failure::Cancelled => end::CANCELLED,
        Failure::Connection(C::Tailnet(error)) => tailnet_end(error),
        Failure::Connection(C::Session(_)) => end::HOST_REFUSED,
        Failure::Observation(_) => end::SESSION_FAILED,
        _ => end::INTERNAL,
    }
}

/// The app's end code for a failed named connection (sessions and the
/// directory fetch alike).
pub(crate) const fn tailnet_end(error: fr_tailnet::Error) -> u32 {
    use fr_tailnet::Error as T;
    match error {
        T::NameUnresolved => end::NAME_UNRESOLVED,
        T::OutsideTailnet => end::OUTSIDE_TAILNET,
        T::IdentityChanged => end::IDENTITY_CHANGED,
        T::NativeHandshake | T::PeerCertificateRejected => end::TLS_REFUSED,
        T::HandshakeTimeout | T::NativeRouteIo | T::NativeBind | T::Timeout => end::UNREACHABLE,
        T::HostBusy => end::HOST_BUSY,
        _ => end::SESSION_FAILED,
    }
}

/// An expected close the client itself asked for. A failed cleanup remains
/// adverse even when the peer names the requested reason.
pub(super) fn requested_terminal(error: ObserverError) -> bool {
    match error {
        ObserverError::Streaming(StreamingViewerError::Control(
            ControlledViewerError::LeaseRevoked(report),
        )) => {
            matches!(
                report.reason,
                fr_wire::lease_revoked::Reason::ClientRequested
                    | fr_wire::lease_revoked::Reason::SessionEnded
            ) && report.cleanup != fr_wire::lease_revoked::CleanupStage::Failed
        }
        ObserverError::Streaming(StreamingViewerError::Session(Error::RemoteClosed(report))) => {
            report.reason == fr_wire::closure::ClosedReason::ClientRequested
                && report.cleanup != fr_wire::closure::Cleanup::Incomplete
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;
