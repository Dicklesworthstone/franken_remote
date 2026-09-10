//! Pure initial-grant transitions share the observation owner's authority.
use super::{Error, ObservationControl, host_now};
use crate::input_agent::{AdmissionGate, SeatReservation};
use asupersync::cx::Cx;
use fr_core::{
    authority::AuthorityError,
    ids::{InputLeaseId, InputTicketId},
    input_submission::InputSession,
};
use fr_wire::control::{Granted, Request};
use std::sync::atomic::Ordering;

impl ObservationControl {
    pub(crate) fn input_origin(&self) -> Result<(Cx, AdmissionGate), Error> {
        self.check()?;
        if let Some(admission) = &self.admission {
            admission.control().map_err(Error::Admission)?;
        }
        Ok((
            self.cx.clone(),
            AdmissionGate {
                tailnet: self.admission.clone(),
            },
        ))
    }
    pub(crate) fn claim_control_grant(&self) -> bool {
        self.control_grant_attached
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    /// Reservation is acquired BEFORE any authority is granted. Readiness must
    /// already come from the qualified view path; a request never creates it.
    pub(crate) fn initial_input(
        &self,
        request: Request,
        input_channel: u32,
        lease: InputLeaseId,
        ticket: InputTicketId,
        _seat: &SeatReservation,
    ) -> Result<(Granted, InputSession), Error> {
        self.input_origin()?;
        let mut granted = false;
        let record = (|| {
            let mut authority = self.authority.lock().map_err(|_| Error::Poisoned)?;
            if authority.session() != request.parent.remote_session {
                return Err(Error::Authority(AuthorityError::StaleLease));
            }
            let now = host_now(&self.cx)?;
            authority
                .grant_lease(lease, now)
                .map_err(Error::Authority)?;
            granted = true;
            let ticket_until = authority
                .issue_input_ticket(lease, ticket, now)
                .map_err(Error::Authority)?;
            let until = authority.control_deadline().map_err(Error::Authority)?;
            Ok(Granted {
                request,
                input_channel,
                lease,
                ticket,
                issued_at_us: now.as_micros(),
                lease_until_us: until.as_micros(),
                ticket_until_us: ticket_until.as_micros(),
                first_action: 0,
                first_pointer: 0,
            })
        })();
        let record = match record {
            Ok(record) => record,
            Err(error) => {
                if granted {
                    self.revoke();
                }
                return Err(error);
            }
        };
        if let Ok(session) = self.input_session(
            record.credentials(),
            request.target.bounds,
            request.target.capabilities,
        ) {
            Ok((record, session))
        } else {
            self.revoke();
            Err(Error::Authority(AuthorityError::NoLease))
        }
    }
}
