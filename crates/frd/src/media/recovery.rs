//! Reference failure joins the original authority, subscription and native source.
use super::{CaptureSource, Error, Subscription, host_now};
use fr_core::{
    authority::{AuthorityError, SessionAuthority},
    time::HostInstant,
};
use fr_media::delivery::{
    DeliveryError, RecoveryDemand, RecoveryDisposition, SendCache, SendError,
};
use fr_wire::decoder::Binding;
use std::sync::Arc;

impl Subscription {
    pub(crate) fn recovery_pending(&self, limits: fr_wire::MediaLimits) -> bool {
        self.cache.needs_recovery() && self.limits == limits
    }

    /// Original failed-chain deadline. No tick through normal egress: that path
    /// deliberately treats `NeedsRecovery` as terminal for callers without recovery.
    pub(crate) fn recovery_deadline(&self) -> Result<u64, Error> {
        let now = self.control.check()?.as_micros();
        let until = self
            .cache
            .next_deadline()
            .filter(|_| self.cache.needs_recovery())
            .ok_or(Error::InvalidFrame)?;
        if now >= until {
            return Err(Error::Send(SendError::Delivery(
                DeliveryError::RecoveryExpired,
            )));
        }
        Ok(until)
    }

    /// Called by the admitted control route after negotiating reference-recovery.
    /// `binding` is the INSTALLED full view binding, never a peer-proposed tuple.
    /// The subscription must already have consumed this actual source's output;
    /// equal numeric frame/configuration IDs cannot select another native worker.
    ///
    /// Acceptance fences old media and shared native input authority BEFORE any
    /// codec work. The existing capture loop services the source's single IDR
    /// queue; it must also wake at `CaptureSource::next_recovery_deadline` when
    /// otherwise idle. Duplicates return false and cannot renew work or time.
    /// Terminal sender refusal also fences view readiness and old input tickets,
    /// but cannot cancel another subscription's queued shared-encoder work.
    ///
    /// This does not grant input, claim presentation, change healthy viewers,
    /// or install fresh channels. The session still admits new bindings and runs
    /// decoder startup before calling `recover` and accepting the resulting IDR.
    pub fn request_recovery(
        &mut self,
        source: &mut CaptureSource,
        bytes: &[u8],
        binding: Binding,
    ) -> Result<bool, Error> {
        // Keep the synchronous API's noninterference guarantee: an unrelated
        // source is refused before changing this subscription's authority/cache.
        self.check_recovery_source(source)?;
        let Some(demand) = self.admit_recovery_request(bytes, binding)? else {
            return Ok(false);
        };
        self.schedule_recovery(source, demand)?;
        Ok(true)
    }

    /// Stage one runs on the network owner while the original source may be
    /// completing a capture. Fence media/input and charge the recovery allowance
    /// NOW, not after a native await. The unique demand retains that deadline.
    pub(crate) fn admit_recovery_request(
        &mut self,
        bytes: &[u8],
        binding: Binding,
    ) -> Result<Option<RecoveryDemand>, Error> {
        self.control.check()?;
        if self.capture_source.is_none() {
            return Err(Error::InvalidFrame);
        }
        let mut authority = self.control.authority.lock().map_err(|_| Error::Poisoned)?;
        let now = host_now(&self.control.cx)?;
        match admit(&mut self.cache, &mut authority, bytes, binding, now)? {
            RecoveryDisposition::Accepted(demand) => Ok(Some(demand)),
            RecoveryDisposition::Coalesced => Ok(None),
        }
    }

    /// Stage two runs only after any old native capture has completed. The
    /// original source AND still-failed cache must match; replacement, mutable
    /// worker escape, expiry and foreign subscriptions cannot redirect the work.
    /// Waiting does not refund admission or buy another recovery budget.
    pub(crate) fn schedule_recovery(
        &self,
        source: &mut CaptureSource,
        demand: RecoveryDemand,
    ) -> Result<(), Error> {
        self.check_recovery_source(source)?;
        source
            .recovery
            .queue(&self.cache, demand, self.control.check()?.as_micros())
            .map_err(Error::Receiver)
    }

    fn check_recovery_source(&self, source: &CaptureSource) -> Result<(), Error> {
        self.control.check()?;
        if source.configuration.generation != self.epoch.configuration
            || self
                .capture_source
                .as_ref()
                .is_none_or(|id| !Arc::ptr_eq(id, &source.source))
            || source
                .selected_control
                .as_ref()
                .is_some_and(|control| !Arc::ptr_eq(&control.authority, &self.control.authority))
        {
            return Err(Error::InvalidFrame);
        }
        Ok(())
    }
}
impl CaptureSource {
    /// The original encoder lifetime owns this one queue and its 500 ms rate
    /// allowance. Generation replacement never recreates or refills it.
    pub fn next_recovery_deadline(&self) -> Option<HostInstant> {
        self.recovery.next_deadline().map(HostInstant::from_micros)
    }
}

/// Called only under the original subscription authority mutex, after source
/// provenance checks. Failed parsing is not permission to disrupt a healthy
/// view. A terminal sender error, however, must fence input BEFORE it escapes
/// this lock, even though no `RecoveryDemand` will be issued to the encoder.
fn admit(
    cache: &mut SendCache,
    authority: &mut SessionAuthority,
    bytes: &[u8],
    binding: Binding,
    now: HostInstant,
) -> Result<RecoveryDisposition, Error> {
    if authority.session() != binding.parent.remote_session {
        return Err(Error::Authority(AuthorityError::StaleLease));
    }
    authority
        .authorize_observation_delivery(now)
        .map_err(Error::Authority)?;
    let result = cache.request_recovery(bytes, binding, now.as_micros());
    // These are the terminal errors from request_recovery. In each case its
    // sender is already closed; issuing input against the old view is unsafe.
    // Malformed/stale records and future-frame claims leave a healthy view alone.
    if matches!(
        result,
        Ok(_)
            | Err(SendError::RecoveryLimitExceeded
                | SendError::Closed
                | SendError::Delivery(
                    DeliveryError::RecoveryExpired
                        | DeliveryError::ClockRegression
                        | DeliveryError::ClockOverflow
                ))
    ) {
        authority.mark_view_stale();
    }
    result.map_err(Error::Send)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::{authority::AuthorityPolicy, ids::*, limits::ProtocolLimits};
    use fr_media::delivery::{MediaBindings, MediaEpoch, SendPolicy};
    use fr_wire::{
        MediaLimits,
        input::{InputDelivery, InputDirection},
        negotiation::ControlBinding,
        recovery_request::{self, Reason, Request},
    };

    fn at(now: u64) -> HostInstant {
        HostInstant::from_micros(now)
    }
    fn binding() -> Binding {
        Binding {
            parent: ControlBinding {
                id: 10,
                host_boot: HostBootId::from_raw(1),
                os_session: OsSessionId::from_raw(2),
                remote_session: RemoteSessionId::from_raw(9),
            },
            display: 4,
            geometry: DisplayGeometryGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
        }
    }
    fn cache() -> SendCache {
        SendCache::new(
            MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16_384, 64).unwrap(),
            MediaBindings::new(1, 2, 3, 4).unwrap(),
            MediaEpoch {
                configuration: binding().configuration,
                recovery: binding().recovery,
            },
            SendPolicy {
                max_recoveries_per_window: 1,
                recovery_horizon_micros: 10_000,
                ..SendPolicy::default()
            },
        )
        .unwrap()
    }
    fn authority() -> SessionAuthority {
        let mut authority = SessionAuthority::new(
            binding().parent.remote_session,
            AuthorityPolicy::plan_defaults(),
        );
        authority.mark_capabilities_checked().unwrap();
        authority.authorize_observation(at(0)).unwrap();
        authority.mark_view_ready(at(0)).unwrap();
        authority
            .grant_lease(InputLeaseId::from_raw(1), at(0))
            .unwrap();
        ready(&mut authority, 1, 0);
        authority
    }
    // Simulates separately admitted fresh-view evidence, not a native decoder.
    fn ready(authority: &mut SessionAuthority, ticket: u128, now: u64) {
        authority.mark_view_ready(at(now)).unwrap();
        authority
            .issue_input_ticket(
                InputLeaseId::from_raw(1),
                InputTicketId::from_raw(ticket),
                at(now),
            )
            .unwrap();
    }
    fn input(
        authority: &mut SessionAuthority,
        ticket: u128,
        now: u64,
    ) -> Result<(), AuthorityError> {
        authority.authorize_submission(
            InputLeaseId::from_raw(1),
            InputTicketId::from_raw(ticket),
            at(now),
        )
    }
    fn request(binding: Binding, frame: Option<u64>) -> Vec<u8> {
        let mut bytes = vec![0; recovery_request::REQUEST_BYTES];
        recovery_request::encode(
            Request {
                reason: Reason::DecodeFailed,
                last_useful_frame: frame,
            },
            binding,
            &ProtocolLimits::ABSOLUTE,
            &mut bytes,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        bytes
    }
    #[test]
    fn chronic_refusal_fences_regranted_input_before_returning_without_touching_other_viewers() {
        let mut cache = cache();
        let mut authority = authority();
        let mut healthy = self::authority();
        let mut binding = binding();
        drop(
            admit(
                &mut cache,
                &mut authority,
                &request(binding, None),
                binding,
                at(0),
            )
            .unwrap(),
        );
        assert!(input(&mut authority, 1, 0).is_err());
        binding.recovery = binding.recovery.next().unwrap();
        cache
            .replace(
                MediaEpoch {
                    configuration: binding.configuration,
                    recovery: binding.recovery,
                },
                MediaBindings::new(11, 12, 13, 14).unwrap(),
                1,
            )
            .unwrap();
        ready(&mut authority, 2, 1);
        input(&mut authority, 2, 1).unwrap();
        assert!(matches!(
            admit(
                &mut cache,
                &mut authority,
                &request(binding, None),
                binding,
                at(2)
            ),
            Err(Error::Send(SendError::RecoveryLimitExceeded))
        ));
        assert!(input(&mut authority, 2, 2).is_err());
        // Restoring readiness alone must not revive an old ticket.
        authority.mark_view_ready(at(3)).unwrap();
        assert!(input(&mut authority, 2, 3).is_err());
        authority.authorize_observation_delivery(at(3)).unwrap();
        input(&mut healthy, 1, 3).unwrap();
        assert_eq!(cache.next_deadline(), None);
        assert_eq!(cache.tick(3), Err(SendError::Closed));
    }
    #[test]
    fn malformed_stale_and_future_frame_requests_preserve_healthy_input() {
        let mut cache = cache();
        let mut authority = authority();
        let binding = binding();
        let mut stale = binding;
        stale.viewport = stale.viewport.next().unwrap();
        for bytes in [vec![0; 8], request(stale, None), request(binding, Some(1))] {
            assert!(admit(&mut cache, &mut authority, &bytes, binding, at(1)).is_err());
            input(&mut authority, 1, 1).unwrap();
            assert!(!cache.needs_recovery());
        }
        // No rejected record spent the one available recovery admission.
        assert!(
            admit(
                &mut cache,
                &mut authority,
                &request(binding, None),
                binding,
                at(2)
            )
            .is_ok()
        );
        assert!(input(&mut authority, 1, 2).is_err());
    }
    #[test]
    fn foreign_installed_session_cannot_stale_a_healthy_authority() {
        let mut cache = cache();
        let mut authority = authority();
        let mut foreign = binding();
        foreign.parent.remote_session = RemoteSessionId::from_raw(100);
        assert!(matches!(
            admit(
                &mut cache,
                &mut authority,
                &request(foreign, None),
                foreign,
                at(1)
            ),
            Err(Error::Authority(AuthorityError::StaleLease))
        ));
        input(&mut authority, 1, 1).unwrap();
        assert!(!cache.needs_recovery());
    }
    #[test]
    fn closed_sender_fences_input_even_when_the_record_cannot_be_parsed() {
        let mut cache = cache();
        let mut authority = authority();
        cache.close();
        assert!(matches!(
            admit(&mut cache, &mut authority, b"bad", binding(), at(1)),
            Err(Error::Send(SendError::Closed))
        ));
        assert!(input(&mut authority, 1, 1).is_err());
        authority.authorize_observation_delivery(at(1)).unwrap();
    }
    #[test]
    fn recovery_deadline_expiry_invalidates_input_without_another_demand() {
        let mut cache = cache();
        let mut authority = authority();
        let binding = binding();
        drop(
            admit(
                &mut cache,
                &mut authority,
                &request(binding, None),
                binding,
                at(0),
            )
            .unwrap(),
        );
        ready(&mut authority, 2, 1);
        assert!(matches!(
            admit(
                &mut cache,
                &mut authority,
                &request(binding, None),
                binding,
                at(10_000)
            ),
            Err(Error::Send(SendError::Delivery(
                DeliveryError::RecoveryExpired
            )))
        ));
        assert!(input(&mut authority, 2, 10_000).is_err());
        assert_eq!(cache.tick(10_000), Err(SendError::Closed));
    }
    #[test]
    fn staged_demand_keeps_admission_deadline_and_input_fence_while_capture_drains() {
        let mut cache = cache();
        let mut authority = authority();
        let binding = binding();
        let RecoveryDisposition::Accepted(demand) = admit(
            &mut cache,
            &mut authority,
            &request(binding, None),
            binding,
            at(100),
        )
        .unwrap() else {
            panic!("new demand")
        };
        let until = demand.deadline_micros();
        assert_eq!(until, 10_100);
        assert!(input(&mut authority, 1, 100).is_err());
        assert!(matches!(cache.tick(101), Err(SendError::NeedsRecovery)));
        // This pause represents finishing the old native operation, not a new
        // timeout. Only the later source scheduling consumes the unique demand.
        let mut queue = fr_media::delivery::IdrCoalescer::new(500_000).unwrap();
        queue.queue(&cache, demand, 9_000).unwrap();
        assert_eq!(queue.take(9_000).unwrap(), Some(until));
        assert_eq!(cache.next_deadline(), Some(until));
        assert!(input(&mut authority, 1, 9_000).is_err());
    }
    #[test]
    fn staged_demand_cannot_cross_expiry_or_failed_cache_replacement() {
        for replace in [false, true] {
            let mut cache = cache();
            let mut authority = authority();
            let binding = binding();
            let RecoveryDisposition::Accepted(demand) = admit(
                &mut cache,
                &mut authority,
                &request(binding, None),
                binding,
                at(0),
            )
            .unwrap() else {
                panic!("new demand")
            };
            if replace {
                cache
                    .replace(
                        MediaEpoch {
                            recovery: binding.recovery.next().unwrap(),
                            configuration: binding.configuration,
                        },
                        MediaBindings::new(11, 12, 13, 14).unwrap(),
                        1,
                    )
                    .unwrap();
            }
            let mut queue = fr_media::delivery::IdrCoalescer::new(500_000).unwrap();
            assert_eq!(
                queue.queue(&cache, demand, if replace { 2 } else { 10_000 }),
                Err(if replace {
                    DeliveryError::StaleGeneration
                } else {
                    DeliveryError::RecoveryExpired
                })
            );
            assert_eq!(queue.next_deadline(), None);
            assert!(input(&mut authority, 1, 10_000).is_err());
        }
    }
}
