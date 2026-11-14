//! Win32 work stays on the original retained consent worker. The native
//! boundary has no Approval, session owner or authority lock. Its display-only
//! labels come from the admitted peer; its answer remains unconsumed until
//! the host turn has rechecked lifecycle and joined this exact worker.
use super::{Error, OPEN_BUDGET, Report, Shared};
use fr_core::indicator_process::{Frame, Kind};
use fr_winsys::approval::Error as Native;
use fr_wire::negotiation::Role;

pub(super) fn run(shared: &Shared) -> Report {
    let details = shared.check().and_then(|()| {
        shared
            .original
            .context()
            .map_err(Error::Original)?
            .details(Frame {
                kind: match shared.original.role() {
                    Role::Observe => Kind::ApproveViewDetails,
                    Role::RequestControl => Kind::ApproveControlDetails,
                },
                sequence: 1,
                // Display-only formatting needs a nonzero correlation value;
                // there is no Windows IPC token or reusable native approval.
                epoch: 1,
            })
            .map_err(|_| Error::Protocol)
    });
    let details = match details {
        Ok(details) => details,
        Err(error) => {
            return Report {
                decision: Err(error),
                cleanup: Ok(()),
            };
        }
    };
    let result = fr_winsys::approval::request(
        &details,
        shared.original.role() == Role::RequestControl,
        |opening| shared.check().is_ok() && (!opening || shared.started.elapsed() < OPEN_BUDGET),
    );
    // Cleanup failure retains the global capacity slot, even if a native yes
    // preceded it. No subsequent request may replace an unretired prompt.
    let cleanup = if result == Err(Native::Cleanup) {
        Err(Error::Cleanup)
    } else {
        Ok(())
    };
    let decision = shared.check().and_then(|()| {
        result.map_err(|error| match error {
            Native::Unavailable => Error::Unavailable,
            Native::Cancelled => Error::Cancelled,
            Native::Expired => Error::Expired,
            Native::Cleanup => Error::Cleanup,
        })
    });
    Report { decision, cleanup }
}
