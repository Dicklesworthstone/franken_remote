//! A local close is intent, not evidence that native or host cleanup succeeded.
//! Kept portable so the macOS completion rules run in the ordinary CLI tests.
use frd::{
    native_connection::{Error as Connection, reconnect::Failure},
    session_startup::{
        ControlledViewerError as Control, Error as Session, ObserverError as Observer,
        StreamingViewerError as Streaming,
    },
};

pub(super) fn checked(
    result: Result<(), Failure>,
    local_close: bool,
    cleanup_confirmed: bool,
    terminal: Option<Observer>,
) -> Result<(), Failure> {
    // Cleanup remains authoritative even if the user closes or interrupts the
    // window while it is failing. No next attempt is safe without this proof.
    if matches!(result, Err(Failure::Cleanup | Failure::CleanupExpired)) {
        return result;
    }
    if !cleanup_confirmed {
        return Err(Failure::Cleanup);
    }
    if let Some(error) = terminal
        && !requested_close(error)
    {
        return Err(Failure::Observation(error));
    }
    match result {
        Err(error) if local_close && local_cancellation(error) => Ok(()),
        other => other,
    }
}

fn requested_close(error: Observer) -> bool {
    use fr_wire::{closure, lease_revoked};
    match error {
        Observer::Session(Session::RemoteClosed(report))
        | Observer::Streaming(
            Streaming::Session(Session::RemoteClosed(report))
            | Streaming::Control(Control::Session(Session::RemoteClosed(report))),
        ) => {
            report.reason == closure::ClosedReason::ClientRequested
                && report.cleanup != closure::Cleanup::Incomplete
        }
        Observer::Streaming(Streaming::Control(Control::LeaseRevoked(report))) => {
            report.reason == lease_revoked::Reason::ClientRequested
                && report.cleanup != lease_revoked::CleanupStage::Failed
        }
        _ => false,
    }
}

pub(super) fn requested_cleanup_failed(error: Failure) -> bool {
    use fr_wire::{closure, lease_revoked};
    match error {
        Failure::Observation(
            Observer::Session(Session::RemoteClosed(report))
            | Observer::Streaming(
                Streaming::Session(Session::RemoteClosed(report))
                | Streaming::Control(Control::Session(Session::RemoteClosed(report))),
            ),
        ) => {
            report.reason == closure::ClosedReason::ClientRequested
                && report.cleanup == closure::Cleanup::Incomplete
        }
        Failure::Observation(Observer::Streaming(Streaming::Control(Control::LeaseRevoked(
            report,
        )))) => {
            report.reason == lease_revoked::Reason::ClientRequested
                && report.cleanup == lease_revoked::CleanupStage::Failed
        }
        _ => false,
    }
}

/// Only the original session's cancellation/closed outcomes can be the expected
/// consequence of a local close. A racing protocol, timeout, codec or callback
/// failure still has its own result; a close click cannot convert it to success.
fn local_cancellation(error: Failure) -> bool {
    use fr_transport::quic::Error as Transport;
    if let Failure::Observation(error) = error
        && requested_close(error)
    {
        return true;
    }
    matches!(
        error,
        Failure::Cancelled
            | Failure::Connection(Connection::Cancelled)
            | Failure::Observation(
                Observer::Transport(Transport::Cancelled)
                    | Observer::Session(
                        Session::Cancelled | Session::Transport(Transport::Cancelled)
                    )
                    | Observer::Streaming(
                        Streaming::Closed
                            | Streaming::Transport(Transport::Cancelled)
                            | Streaming::Session(
                                Session::Cancelled | Session::Transport(Transport::Cancelled)
                            )
                            | Streaming::Control(
                                Control::Closed
                                    | Control::Capture(
                                        frd::session_startup::viewer_events::Error::Closed
                                    )
                                    | Control::Session(
                                        Session::Cancelled
                                            | Session::Transport(Transport::Cancelled)
                                    )
                            )
                            | Streaming::Media(frd::media::Error::Worker(
                                frd::worker::Error::Cancelled
                            ))
                    )
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_wire::closure::{Cleanup, Closed, ClosedReason, OutstandingEffects};

    fn terminal(reason: ClosedReason, cleanup: Cleanup) -> Observer {
        Observer::Streaming(Streaming::Session(Session::RemoteClosed(Closed {
            reason,
            cleanup,
            effects: OutstandingEffects::Unknown,
        })))
    }

    #[test]
    fn closing_the_window_cannot_hide_unconfirmed_local_cleanup() {
        for result in [Ok(()), Err(Failure::Cancelled), Err(Failure::Cleanup)] {
            assert_eq!(checked(result, true, false, None), Err(Failure::Cleanup));
        }
        for failure in [Failure::Cleanup, Failure::CleanupExpired] {
            assert_eq!(checked(Err(failure), true, true, None), Err(failure));
        }
    }

    #[test]
    fn local_close_preserves_host_permission_loss_and_incomplete_cleanup() {
        for report in [
            terminal(ClosedReason::PermissionLost, Cleanup::Complete),
            terminal(ClosedReason::ClientRequested, Cleanup::Incomplete),
        ] {
            for result in [Ok(()), Err(Failure::Cancelled)] {
                assert_eq!(
                    checked(result, true, true, Some(report)),
                    Err(Failure::Observation(report))
                );
            }
            assert_eq!(
                requested_cleanup_failed(Failure::Observation(report)),
                report == terminal(ClosedReason::ClientRequested, Cleanup::Incomplete)
            );
        }
    }

    #[test]
    fn closing_does_not_hide_a_racing_protocol_or_native_failure() {
        for failure in [
            Failure::Notification,
            Failure::Clock,
            Failure::Observation(Observer::Application),
            Failure::Observation(Observer::Streaming(Streaming::UnclaimedRecord(0x0054))),
            Failure::Observation(Observer::Streaming(Streaming::Media(
                frd::media::Error::Worker(frd::worker::Error::Deadline),
            ))),
        ] {
            assert_eq!(checked(Err(failure), true, true, None), Err(failure));
        }
    }

    #[test]
    fn cancellation_is_success_only_after_a_local_close_and_native_cleanup() {
        for failure in [
            Failure::Cancelled,
            Failure::Connection(Connection::Cancelled),
            Failure::Observation(Observer::Streaming(Streaming::Control(Control::Closed))),
            Failure::Observation(Observer::Streaming(Streaming::Transport(
                fr_transport::quic::Error::Cancelled,
            ))),
        ] {
            assert_eq!(checked(Err(failure), false, true, None), Err(failure));
            assert_eq!(checked(Err(failure), true, true, None), Ok(()));
        }
        let report = terminal(ClosedReason::ClientRequested, Cleanup::Unconfirmed);
        assert_eq!(checked(Ok(()), true, true, Some(report)), Ok(()));
        assert_eq!(
            checked(Err(Failure::Observation(report)), true, true, Some(report)),
            Ok(())
        );
    }

    #[test]
    fn a_host_revoke_wins_over_local_close_but_not_over_failed_native_cleanup() {
        use fr_wire::lease_revoked::{CleanupStage, EffectStage, Reason, Revoked};
        for (reason, cleanup) in [
            (Reason::LocalRevoke, CleanupStage::Released),
            (Reason::ClientRequested, CleanupStage::Failed),
        ] {
            let report = Observer::Streaming(Streaming::Control(Control::LeaseRevoked(Revoked {
                lease: fr_core::ids::InputLeaseId::new(7),
                reason,
                cleanup,
                effects: EffectStage::ReceiptsPending,
            })));
            assert_eq!(
                checked(Err(Failure::Cancelled), true, true, Some(report)),
                Err(Failure::Observation(report))
            );
            assert_eq!(
                checked(Ok(()), true, false, Some(report)),
                Err(Failure::Cleanup)
            );
            assert_eq!(
                requested_cleanup_failed(Failure::Observation(report)),
                reason == Reason::ClientRequested
            );
        }
    }
}
