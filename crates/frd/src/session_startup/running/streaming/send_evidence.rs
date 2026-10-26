//! Bounded enqueue evidence for one original video-service generation.
//! Polling an empty sender is not an admission outcome. Reads keep the actual
//! outcome's timestamp; recovery starts a fresh owner with no inherited sample.
use fr_media::pacing::{Availability, SOURCE_EVIDENCE_US};
use fr_wire::decoder::Binding;

#[derive(Clone, Copy)]
struct Sample {
    view: Binding,
    at_us: u64,
    availability: Availability,
}

#[derive(Default)]
pub(super) struct SendEvidence {
    sample: Option<Sample>,
}
impl SendEvidence {
    pub(super) fn accepted(&mut self, view: Binding, at_us: u64) {
        self.sample = Some(Sample {
            view,
            at_us,
            availability: Availability::Ready,
        });
    }

    pub(super) fn blocked(&mut self, view: Binding, at_us: u64) {
        self.sample = Some(Sample {
            view,
            at_us,
            availability: Availability::Blocked,
        });
    }

    pub(super) fn current(&self, view: Binding, now_us: u64, interval_us: u64) -> Availability {
        // A real enqueue can occur only once per slowed capture interval. Keep
        // that evidence across one cadence, never beyond the source horizon.
        let lifetime = interval_us.clamp(100_000, SOURCE_EVIDENCE_US);
        self.sample
            .filter(|sample| {
                sample.view == view
                    && now_us
                        .checked_sub(sample.at_us)
                        .is_some_and(|age| age <= lifetime)
            })
            .map_or(Availability::Unknown, |sample| sample.availability)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::ids::*;
    use fr_media::pacing::{Controller, Observation, Policy, Reason};

    fn view() -> Binding {
        Binding {
            parent: fr_wire::negotiation::ControlBinding {
                id: 1,
                host_boot: HostBootId::from_raw(2),
                os_session: OsSessionId::from_raw(3),
                remote_session: RemoteSessionId::from_raw(4),
            },
            display: 5,
            geometry: DisplayGeometryGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
        }
    }

    fn controller() -> Controller {
        Controller::new(Policy {
            minimum_interval_us: 50_000,
            maximum_interval_us: 200_000,
        })
        .unwrap()
    }

    #[test]
    fn reads_never_refresh_an_actual_outcome_and_each_cadence_has_a_fixed_bound() {
        let view = view();
        for (interval, lifetime) in [(50_000, 100_000), (200_000, 200_000), (1_000_000, 250_000)] {
            let mut evidence = SendEvidence::default();
            assert_eq!(evidence.current(view, 0, interval), Availability::Unknown);
            for blocked in [false, true] {
                if blocked {
                    evidence.blocked(view, 1000);
                } else {
                    evidence.accepted(view, 1000);
                }
                let expected = if blocked {
                    Availability::Blocked
                } else {
                    Availability::Ready
                };
                for at in [1000, 50_000, 1000 + lifetime] {
                    assert_eq!(evidence.current(view, at, interval), expected);
                }
                assert_eq!(evidence.current(view, 999, interval), Availability::Unknown);
                assert_eq!(
                    evidence.current(view, 1001 + lifetime, interval),
                    Availability::Unknown
                );
            }
        }
    }

    #[test]
    fn the_latest_real_outcome_wins_without_carrying_evidence_across_any_view_change() {
        let original = view();
        let mut evidence = SendEvidence::default();
        evidence.accepted(original, 10_000);
        evidence.blocked(original, 20_000);
        assert_eq!(
            evidence.current(original, 20_001, 100_000),
            Availability::Blocked
        );
        evidence.accepted(original, 30_000);
        assert_eq!(
            evidence.current(original, 30_001, 100_000),
            Availability::Ready
        );
        let mut changed = [original; 9];
        changed[0].parent.id += 1;
        changed[1].parent.host_boot = HostBootId::from_raw(20);
        changed[2].parent.os_session = OsSessionId::from_raw(30);
        changed[3].parent.remote_session = RemoteSessionId::from_raw(40);
        changed[4].display += 1;
        changed[5].geometry = original.geometry.next().unwrap();
        changed[6].configuration = original.configuration.next().unwrap();
        changed[7].recovery = original.recovery.next().unwrap();
        changed[8].viewport = original.viewport.next().unwrap();
        for view in changed {
            assert_eq!(
                evidence.current(view, 30_001, 100_000),
                Availability::Unknown
            );
        }
        assert_eq!(
            SendEvidence::default().current(original, 30_001, 100_000),
            Availability::Unknown,
            "a new service round cannot inherit an earlier generation's outcome"
        );
    }

    #[test]
    fn fresh_source_work_without_any_enqueue_cannot_accumulate_probe_headroom() {
        let evidence = SendEvidence::default();
        let mut pacing = controller();
        for now in (0_u64..=2_500_000).step_by(10_000) {
            let report = pacing
                .update(fr_media::pacing::Sample {
                    now_us: now,
                    source_work_us: Some(1000),
                    send: evidence.current(view(), now, pacing.interval_us()),
                    capture_credit: Availability::Ready,
                    observation: now.is_multiple_of(100_000).then_some(Observation {
                        at_us: now,
                        changed: true,
                    }),
                })
                .unwrap();
            assert_eq!(report.headroom_us, 0);
        }
        assert_eq!(pacing.interval_us(), 100_000);
        assert!(
            !pacing
                .decisions()
                .any(|r| r.reason == Reason::HeadroomProbe)
        );
    }

    #[test]
    fn actual_cadence_enqueues_recover_after_backpressure_with_no_idle_refresh() {
        let mut evidence = SendEvidence::default();
        let mut pacing = controller();
        let mut last_capture = None;
        let mut captures = 0;
        for now in (0_u64..=3_500_000).step_by(10_000) {
            if now <= 300_000 {
                evidence.blocked(view(), now);
            }
            let due = last_capture.map_or(0, |last| last + pacing.interval_us() + 10_000);
            let observation = if now >= due {
                last_capture = Some(now);
                captures += 1;
                if now > 300_000 {
                    evidence.accepted(view(), now);
                }
                Some(Observation {
                    at_us: now,
                    changed: true,
                })
            } else {
                None
            };
            // The maintenance reads between real capture/enqueue turns do not
            // create new evidence. A fixed 100 ms expiry strands this 200 ms cadence.
            pacing
                .update(fr_media::pacing::Sample {
                    now_us: now,
                    source_work_us: Some(1000),
                    send: evidence.current(view(), now, pacing.interval_us()),
                    capture_credit: Availability::Ready,
                    observation,
                })
                .unwrap();
            if now == 300_000 {
                assert_eq!(pacing.interval_us(), 200_000);
            }
        }
        assert!((10..30).contains(&captures));
        assert!(pacing.interval_us() < 200_000);
        assert!(
            pacing
                .decisions()
                .any(|r| r.reason == Reason::SendAdmission)
        );
        assert!(
            pacing
                .decisions()
                .any(|r| r.reason == Reason::HeadroomProbe)
        );
    }
}
