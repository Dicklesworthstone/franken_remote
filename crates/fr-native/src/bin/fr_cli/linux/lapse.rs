//! Name the local session ends the client can observe without a host report.
//! Host refusals and revocation reports are mapped first (refusal.rs);
//! this never infers a host reason, retries, or claims anything was undone.
use super::{Failure, failure};
use frd::native_connection::reconnect::{self, Lapse};

pub(super) fn reconnect(error: reconnect::Failure) -> Option<Failure> {
    Some(match reconnect::lapse(error)? {
        Lapse::TransportDeadline => failure(
            "transport_deadline_expired",
            "A session record was not delivered within its transport deadline, so the connection was closed instead of delivering late state; nothing is replayed. Check the path's round-trip time and loss (tailscale ping) before starting a new session.",
        ),
        Lapse::ViewStale => failure(
            "view_stale",
            "The picture on screen could no longer be proven fresh within the source-age limit, so input stopped instead of acting on a stale view. Control is not reacquired automatically and submitted actions are not rolled back. Check the path's round-trip time and loss (tailscale ping) before starting a new session.",
        ),
        Lapse::HostNotHeard => failure(
            "host_not_heard",
            "The host's renewals did not arrive within this session's deadline, so the session ended instead of continuing unrenewed; nothing is replayed. Check the path's round-trip time and loss (tailscale ping) and that the host is still running.",
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_client::input::presentation::Error as View;
    use fr_media::freshness::Error as Freshness;
    use fr_transport::quic::Error as Transport;
    use frd::session_startup::{
        ControlledViewerError as Control, Error as Session, ObserverError,
        StreamingViewerError as Streaming,
    };

    fn observed(error: Streaming) -> reconnect::Failure {
        reconnect::Failure::Observation(ObserverError::Streaming(error))
    }

    #[test]
    fn each_lapse_has_a_distinct_code_and_exit_one() {
        let deadline = reconnect(observed(Streaming::Control(Control::Session(
            Session::Transport(Transport::Expired),
        ))))
        .unwrap();
        let stale = reconnect(observed(Streaming::Control(Control::View(View::Media(
            Freshness::SourceStale,
        )))))
        .unwrap();
        let silent = reconnect(observed(Streaming::Session(Session::Expired))).unwrap();
        assert_eq!(deadline.code, "transport_deadline_expired");
        assert_eq!(stale.code, "view_stale");
        assert_eq!(silent.code, "host_not_heard");
        for failure in [deadline, stale, silent] {
            assert_eq!(failure.exit, 1);
            assert_eq!(failure.revocation, None);
            assert!(failure.next.contains("tailscale ping"));
        }
    }

    #[test]
    fn other_local_failures_keep_the_generic_path() {
        for error in [
            observed(Streaming::Transport(Transport::Closed)),
            observed(Streaming::Control(Control::View(View::Media(
                Freshness::QueueExpired,
            )))),
            observed(Streaming::Control(Control::Expired)),
            reconnect::Failure::Observation(ObserverError::Expired),
            reconnect::Failure::Cleanup,
            reconnect::Failure::Cancelled,
        ] {
            assert_eq!(reconnect(error), None, "{error:?}");
        }
    }
}
