//! Actual TLS, request decoding, native Driver and GrantServices queue boundary.
//! The common-clock readiness, local consent and native sink are fixtures.
use super::*;
use crate::session_startup::{closure_reporting::Reporting, running::controlled::revocation};
use fr_transport::quic;
use fr_wire::{
    authority::Binding,
    closure::ClosedReason,
    input::{InputDelivery, InputDirection},
    lease_revoked::{self, CleanupStage, EffectStage, Reason, Revoked},
};

struct CloseQueued {
    original: quic::ConnectionBinding,
    route: quic::StreamRoute,
    binding: Binding,
    previous: quic::ClosedRegistration,
    registration: quic::RevocationRegistration,
    reached: bool,
}
impl Services for CloseQueued {
    fn maintain<N: FnMut() -> Result<u128, ()>>(
        &mut self,
        transport: &mut QuicRecords,
        _: &mut N,
    ) -> Result<(), Error> {
        match transport.confirm_revocation_report(
            &self.original,
            self.route,
            self.binding,
            InputLeaseId::from_raw(19),
            Some(&self.previous),
            &self.registration,
        ) {
            Err(quic::Error::InvalidPolicy) => Ok(()), // Grant not queued yet.
            Ok(()) => {
                assert!(!self.reached, "one queued grant boundary");
                self.reached = true;
                transport.close();
                Err(Error::Closed)
            }
            Err(error) => panic!("foreign or stale reporting handoff: {error:?}"),
        }
    }
    fn receive(&mut self, _: Route, _: &[u8]) -> Result<Disposition, ()> {
        Ok(Disposition::Blocked)
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn queued_grant_closure_before_broker_finish_has_only_the_original_lease_report() {
    run3(|c, h, cleanup| async move {
        let (closed, configuration) = Reporting::new(&cleanup);
        let mut capabilities: Vec<_> = [
            fr_wire::decoder::CAPABILITY,
            attachment::CAPABILITY,
            attachment::DELIVERY_CAPABILITY,
            attachment::INPUT_CAPABILITY,
            crate::input_quic::grant::CAPABILITY,
        ]
        .into_iter()
        .map(|name| WireCapability {
            name: name.into(),
            version: 1,
            required: true,
        })
        .collect();
        capabilities.sort_by(|a, b| a.name.cmp(&b.name));
        let (mut host, viewer) = Box::pin(
            crate::session_startup::running::tests::pair_before_finish_initialized(
                &c,
                &h,
                capabilities,
                |host| host.retain_observation_reporting(configuration).unwrap(),
            ),
        )
        .await;
        host.authority
            .as_mut()
            .unwrap()
            .mark_view_ready(HostInstant::from_micros(now(&h).unwrap()))
            .unwrap();
        let mut host = host.finish().unwrap().into_running().unwrap();
        let mut viewer = viewer.finish().unwrap();
        let (hc, _vc) = attach(&mut host, &mut viewer, &c, &h, MediaRole::Configuration, 8).await;
        let (hi, _vi) = attach(&mut host, &mut viewer, &c, &h, MediaRole::Input, 10).await;
        let selection = host.selection().clone();
        let channels = NegotiatedInput::new(host.io().unwrap().0, &selection, &hc, hi).unwrap();
        let seat = Seat::default();
        let mut broker = host
            .negotiated_control_broker(seat.clone(), channels)
            .unwrap();
        let request = Request {
            parent: host.binding(),
            sequence: 1,
            target: target(),
        };
        let mut bytes = [0; 512];
        let length = fr_wire::control::encode_request(
            request,
            &mut bytes,
            &selection.limits,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        let (q, routes) = viewer.io().unwrap();
        q.send(
            &c,
            Route::Stream(routes.outbound),
            &bytes[..length],
            now(&c).unwrap() + 500_000,
            || true,
        )
        .unwrap();
        let mut nonce = 30_000;
        while broker.request().is_none() {
            let (a, b) = Box::pin(support::both(
                host.drive(
                    Duration::from_millis(1),
                    || {
                        nonce += 1;
                        Ok(nonce)
                    },
                    |route, bytes| {
                        broker
                            .dispatch_record(route, bytes)
                            .map(|handled| handled.unwrap_or(Disposition::Blocked))
                            .map_err(|_| ())
                    },
                ),
                viewer.drive(Duration::from_millis(1), block),
            ))
            .await;
            a.unwrap();
            b.unwrap();
        }
        // Settle attachment and request payloads before the grant. This tests
        // the one typed record, separately from the native-backlog close path.
        for _ in 0..8 {
            let (a, b) = Box::pin(support::both(
                host.drive(
                    Duration::from_millis(1),
                    || {
                        nonce += 1;
                        Ok(nonce)
                    },
                    block,
                ),
                viewer.drive(Duration::from_millis(1), block),
            ))
            .await;
            a.unwrap();
            b.unwrap();
        }
        let effects = Arc::new(Mutex::new(Vec::new()));
        let sink = effects.clone();
        let driver = broker
            .approve(
                host.io().unwrap().0,
                target(),
                || Some((InputLeaseId::from_raw(19), InputTicketId::from_raw(23))),
                move || Ok(Sink(sink)),
                |_| true,
            )
            .unwrap();
        let revoked = quic::RevocationReport::default();
        host.revocation_reporting = Some(revocation::Configuration {
            cleanup: cleanup.clone(),
            registration: revoked.registration(),
            armed: false,
        });
        let mut close = CloseQueued {
            original: host.io().unwrap().0.binding(),
            route: host.opened.routes.outbound,
            binding: Binding {
                channel: host.binding().id,
                session: host.binding().remote_session,
            },
            previous: host.opened.pregrant_reporting.clone().unwrap(),
            registration: revoked.registration(),
            reached: false,
        };
        let mut acquisition = Acquisition {
            broker: Some(broker),
            reporting: GrantReporting::take(&mut host).unwrap(),
            request: Some(request),
            active: None,
            current: Some(target()),
            ready: false,
            failure: None,
        };
        let mut owner = Host::Observe(host);
        let ((result, delivery, records), native) = Box::pin(support::both(
            async {
                let result = loop {
                    let (a, b) = Box::pin(support::both(
                        acquisition.drive(
                            &mut owner,
                            Duration::from_millis(1),
                            &mut || {
                                nonce += 1;
                                Ok(nonce)
                            },
                            &mut || panic!("no steady-state input"),
                            &mut close,
                            &mut |_| Ok(Some(target())),
                        ),
                        viewer.drive(Duration::from_millis(1), block),
                    ))
                    .await;
                    b.unwrap();
                    if let Err(error) = a {
                        break error;
                    }
                };
                assert!(
                    acquisition.ready && acquisition.broker.is_some(),
                    "before broker.finish"
                );
                assert!(acquisition.active.is_none());
                assert_eq!(closed.finish(ClosedReason::HostFailure).await, None);
                let done = Cell::new(false);
                let mut records = Vec::new();
                let (delivery, ()) = Box::pin(support::both(
                    async {
                        let result = revoked.finish().await;
                        done.set(true);
                        result
                    },
                    async {
                        while !done.get() {
                            viewer
                                .drive(Duration::from_millis(1), |_, bytes| {
                                    records.push(
                                        lease_revoked::decode(
                                            bytes,
                                            close.binding,
                                            InputLeaseId::from_raw(19),
                                            &selection.limits,
                                            InputDirection::HostToViewer,
                                            InputDelivery::Reliable,
                                        )
                                        .unwrap(),
                                    );
                                    Ok(Disposition::Consumed)
                                })
                                .await
                                .unwrap();
                        }
                    },
                ))
                .await;
                (result, delivery, records)
            },
            driver,
        ))
        .await;
        assert_eq!(result, Error::Closed);
        assert!(close.reached);
        assert_eq!(delivery, Some(Ok(())));
        assert_eq!(
            records,
            [Revoked {
                lease: InputLeaseId::from_raw(19),
                reason: Reason::SessionEnded,
                cleanup: CleanupStage::Fenced,
                effects: EffectStage::Unknown
            }]
        );
        assert!(native.handoff_safe());
        assert!(!seat.is_occupied());
        assert!(effects.lock().unwrap().is_empty());
        assert!(!cleanup.is_cancel_requested());
        viewer.close();
    });
}
