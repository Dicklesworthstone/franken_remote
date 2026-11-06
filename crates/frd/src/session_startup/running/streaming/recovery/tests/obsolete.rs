//! Actual session/media attachments and the existing late-capture subprocess.
//! Exercise both private recovery dispatchers; no codec or hardware claim.
use super::*;
use fr_wire::{
    input::{InputDelivery, InputDirection},
    presented as wire_presented, receiver_metrics as wire_metrics,
};

struct Records {
    view: Binding,
    limits: fr_wire::MediaLimits,
    progress: fr_wire::Progress,
    inbound: Route,
    repair: Route,
}
impl Records {
    fn new(host: &mut StreamingHost, progress: fr_wire::Progress) -> Self {
        let session = host.host.session().unwrap();
        let inbound = Route::Stream(session.opened.routes.inbound);
        let parent = session.opened.binding;
        let media = host.media.as_ref().unwrap();
        let mut view = media.binding();
        view.parent = parent;
        Self {
            view,
            limits: media.limits(),
            progress,
            inbound,
            repair: host.stream.sender.stream_repair_route(),
        }
    }
    fn presentation(&self, view: Binding) -> Vec<u8> {
        let mut bytes = [0; wire_presented::BYTES];
        let len = wire_presented::encode(
            wire_presented::Report {
                sequence: 1,
                visible: Some(wire_presented::Sample {
                    stamp: wire_presented::Stamp {
                        frame: self.progress.descriptor.frame,
                        captured_us: self.progress.descriptor.capture_micros,
                        observed_us: self.progress.observed_micros,
                        source: self.progress.observation,
                    },
                    age_upper_us: 0,
                }),
            },
            view,
            self.limits.protocol(),
            &mut bytes,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        bytes[..len].to_vec()
    }
    fn metrics(&self, view: Binding, message: wire_metrics::Message) -> Vec<u8> {
        let mut bytes = [0; wire_metrics::REPLY_BYTES];
        let direction = match message {
            wire_metrics::Message::Query { .. } => InputDirection::HostToViewer,
            wire_metrics::Message::Reply { .. } => InputDirection::ViewerToHost,
        };
        let len = wire_metrics::encode(
            message,
            view,
            self.limits.protocol(),
            &mut bytes,
            direction,
            InputDelivery::Reliable,
        )
        .unwrap();
        bytes[..len].to_vec()
    }
    fn reply(&self, view: Binding) -> Vec<u8> {
        self.metrics(
            view,
            wire_metrics::Message::Reply {
                sequence: 1,
                load: wire_metrics::Load {
                    retained_bytes: 0,
                    retained_pictures: 0,
                    decoding: false,
                    work_us: None,
                },
            },
        )
    }
    fn repair(&self) -> Vec<u8> {
        let Route::Stream(route) = self.repair else {
            panic!("repair must retain its original reliable channel");
        };
        let mut bytes = vec![0; self.limits.record_bytes()];
        let len = fr_wire::encode_repair(
            self.progress.descriptor.frame,
            &[fr_wire::RepairRange { start: 0, end: 1 }],
            self.limits.max_fragments(),
            route.binding,
            &self.limits,
            &mut bytes,
        )
        .unwrap();
        bytes.truncate(len);
        bytes
    }
    fn request(&self) -> Vec<u8> {
        let mut bytes = [0; recovery_request::REQUEST_BYTES];
        let len = recovery_request::encode(
            recovery_request::Request {
                reason: recovery_request::Reason::ReferenceExpired,
                last_useful_frame: None,
            },
            self.view,
            self.limits.protocol(),
            &mut bytes,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        bytes[..len].to_vec()
    }
    fn valid(&self, services: &mut impl Services, reports_selected: bool) {
        for bytes in [self.presentation(self.view), self.reply(self.view)] {
            assert_eq!(
                services.receive(self.inbound, &bytes),
                if reports_selected {
                    Ok(Disposition::Consumed)
                } else {
                    Err(())
                },
                "recovery must retain the original optional capability selection"
            );
        }
        assert_eq!(
            services.receive(self.repair, &self.repair()),
            Ok(Disposition::Consumed)
        );
    }
    fn refused(&self, services: &mut impl Services) {
        // Each component belongs to the original compound view. The outer
        // session binding alone must not admit a different source/generation.
        let mut foreign = [self.view; 9];
        foreign[0].parent.id += 1;
        foreign[1].parent.host_boot = HostBootId::from_raw(self.view.parent.host_boot.as_raw() + 1);
        foreign[2].parent.os_session =
            OsSessionId::from_raw(self.view.parent.os_session.as_raw() + 1);
        foreign[3].parent.remote_session =
            RemoteSessionId::from_raw(self.view.parent.remote_session.as_raw() + 1);
        foreign[4].display += 1;
        foreign[5].geometry = self.view.geometry.next().unwrap();
        foreign[6].configuration = self.view.configuration.next().unwrap();
        foreign[7].recovery = self.view.recovery.next().unwrap();
        foreign[8].viewport = self.view.viewport.next().unwrap();
        for view in foreign {
            for bytes in [self.presentation(view), self.reply(view)] {
                assert!(services.receive(self.inbound, &bytes).is_err());
            }
        }
        let mut presentation = self.presentation(self.view);
        presentation[fr_wire::HEADER_BYTES + fr_wire::decoder::BINDING_BYTES] = 2;
        assert!(services.receive(self.inbound, &presentation).is_err());
        let mut metrics = self.reply(self.view);
        metrics[fr_wire::HEADER_BYTES + fr_wire::decoder::BINDING_BYTES] = 2;
        assert!(services.receive(self.inbound, &metrics).is_err());
        let query = self.metrics(self.view, wire_metrics::Message::Query { sequence: 1 });
        assert!(services.receive(self.inbound, &query).is_err());
        // A repair must parse, including its original channel and range limit.
        // These payload errors pass the transport's generic record framing.
        let mut repair = self.repair();
        let end = fr_wire::HEADER_BYTES + 12 + 4;
        repair[end..end + 4].copy_from_slice(&(self.limits.max_fragments() + 1).to_be_bytes());
        assert!(services.receive(self.repair, &repair).is_err());
        let Route::Stream(route) = self.repair else {
            unreachable!();
        };
        repair = self.repair();
        repair[16..20].copy_from_slice(&(route.binding + 1).to_be_bytes());
        assert!(services.receive(self.repair, &repair).is_err());
        for bytes in [self.presentation(self.view), self.reply(self.view)] {
            assert!(services.receive(self.repair, &bytes).is_err());
        }
        assert!(services.receive(self.repair, b"not a repair").is_err());
    }
}

/// No capture producer runs here: an already-failed sender must enter recovery
/// before it can request another capture or feed its empty source to presentation.
fn admission<'a, S: Services>(
    host: &'a mut StreamingHost,
    other: &'a mut S,
) -> (&'a mut HostSession, Admission<'a, S>) {
    let session = host.host.session().unwrap();
    let stream = &mut host.stream;
    let (credit, _) = mpsc::channel(1);
    let (_, results) = mpsc::channel(1);
    let services = Admission {
        media: host.media.as_ref(),
        routes: session.opened.routes,
        parent: session.opened.binding,
        pending: None,
        local_failure: None,
        video: VideoServices {
            control: &stream.control,
            sender: &mut stream.sender,
            statistics: &mut stream.statistics,
            policy: stream.policy,
            capacity: stream.capacity,
            credit,
            results,
            in_flight: None,
            native_target_check: None,
            send_evidence: send_evidence::SendEvidence::default(),
            pacing: stream.pacing.as_mut(),
            bitrate: stream.bitrate.as_mut(),
            rate_in_flight: None,
            last_work: None,
            last_capture: None,
            next_capture: 0,
            input_wake: crate::session_startup::running::input_wake::Wake::default(),
            feedback: host.feedback.as_mut(),
            quality: None,
            presentation: host.presentation.as_mut(),
            cursor: None,
            media: host.media.as_ref(),
            other,
        },
    };
    (session, services)
}

async fn request<S: Services>(
    c: &Cx,
    session: &mut HostSession,
    viewer: &mut ViewerSession,
    admission: &mut Admission<'_, S>,
    records: &Records,
    nonce: &mut impl FnMut() -> Result<u128, ()>,
) {
    let bytes = records.request();
    let deadline = now(c).unwrap() + 1_000_000;
    let mut sent = false;
    while admission.pending.is_none() {
        assert!(now(c).unwrap() < deadline, "bound recovery request stalled");
        if !sent {
            let (q, routes) = viewer.io().unwrap();
            match q.send(c, Route::Stream(routes.outbound), &bytes, deadline, || true) {
                Ok(()) => sent = true,
                Err(fr_transport::quic::Error::Backpressure) => {}
                Err(error) => panic!("request admission failed: {error:?}"),
            }
        }
        let (host, peer) = Box::pin(support::both(
            session.drive_inner(Duration::from_millis(1), nonce, admission),
            viewer.drive(Duration::from_millis(1), super::super::super::tests::block),
        ))
        .await;
        host.unwrap();
        peer.unwrap();
    }
    assert!(admission.local_failure.is_none());
}

async fn refused_on_connection(
    c: &Cx,
    session: &mut HostSession,
    viewer: &mut ViewerSession,
    services: &mut impl Services,
    bytes: &[u8],
    nonce: &mut impl FnMut() -> Result<u128, ()>,
) {
    let deadline = now(c).unwrap() + 1_000_000;
    let mut sent = false;
    loop {
        assert!(
            now(c).unwrap() < deadline,
            "invalid report was silently consumed"
        );
        if !sent {
            let (q, routes) = viewer.io().unwrap();
            match q.send(c, Route::Stream(routes.outbound), bytes, deadline, || true) {
                Ok(()) => sent = true,
                Err(fr_transport::quic::Error::Backpressure) => {}
                Err(error) => panic!("report admission failed: {error:?}"),
            }
        }
        let (host, peer) = Box::pin(support::both(
            session.drive_inner(Duration::from_millis(1), nonce, services),
            viewer.drive(Duration::from_millis(1), super::super::super::tests::block),
        ))
        .await;
        if let Err(error) = host {
            assert_eq!(
                error,
                Error::Renewal(crate::media::renewal::Error::Transport(
                    fr_transport::quic::Error::Handler,
                ))
            );
            assert!(session.opened.transport.is_closed());
            assert!(session.opened.control.revoked());
            return;
        }
        peer.unwrap();
    }
}

#[test]
fn obsolete_reports_keep_the_original_scope_through_local_failure_and_replacement() {
    exercise(true);
}
#[test]
fn recovery_does_not_enable_unselected_presentation_or_receiver_metrics() {
    exercise(false);
}

#[allow(clippy::too_many_lines)]
fn exercise(reports_selected: bool) {
    run(move |c, h| async move {
        let cleanup = Cx::current().unwrap();
        let (
            Fixture {
                mut host,
                mut viewer,
                mut presenter,
                ..
            },
            _input,
        ) = Box::pin(fixture_with_input(
            &c,
            &h,
            "late-once",
            2_000_000,
            if reports_selected {
                Role::RequestControl
            } else {
                Role::Observe
            },
            true,
            reports_selected,
            reports_selected,
        ))
        .await;
        assert_eq!(host.presentation.is_some(), reports_selected);
        assert_eq!(host.feedback.is_some(), reports_selected);
        let control = host.stream.control.clone();
        let worker = host.worker_id();
        let progress = host.stream.sender.source_progress().unwrap().unwrap();
        let records = Records::new(&mut host, progress);
        let reports_before = (
            host.presentation_reports(),
            host.receiver_feedback_reports(),
        );
        let update = host
            .stream
            .source
            .capture_if_changed(&control, false)
            .await
            .unwrap();
        assert!(expired_original(
            &host.stream.sender.enqueue_capture(update).unwrap_err()
        ));
        let mut entropy = 80_000;
        let mut nonce = || super::super::super::tests::nonce(&mut entropy);
        let mut other = super::super::super::tests::block;
        let (demand, until) = {
            let (session, mut services) = admission(&mut host, &mut other);
            services
                .maintain(&mut session.opened.transport, &mut nonce)
                .unwrap();
            let until = services.local_failure.as_ref().unwrap().until;
            assert!(!services.video.sender.is_closed());
            assert!(!session.opened.transport.is_closed());
            assert!(!control.view_ready().unwrap());
            records.valid(&mut services, reports_selected);
            // Direct callback assertions isolate refusal at the same production
            // dispatch seam. The connection owner's terminal handling is checked
            // below with an actual incoming record over TLS/UDP.
            records.refused(&mut services);
            assert_eq!(services.local_failure.as_ref().unwrap().until, until);
            assert!(services.video.sender.source_progress().unwrap().is_none());
            assert_eq!(services.video.sender.cache_usage().bytes, 0);
            assert_eq!(
                (
                    services
                        .video
                        .presentation
                        .as_ref()
                        .map_or(0, |p| p.accepted),
                    services.video.feedback.as_ref().map_or(0, |f| f.accepted),
                ),
                reports_before
            );
            request(
                &c,
                session,
                &mut viewer,
                &mut services,
                &records,
                &mut nonce,
            )
            .await;
            assert_eq!(services.pending.as_ref().unwrap().deadline_micros(), until);
            records.valid(&mut services, reports_selected);
            assert_eq!(services.pending.as_ref().unwrap().deadline_micros(), until);
            (services.pending.take().unwrap(), until)
        };
        assert_eq!(host.stream.source.next_recovery_deadline(), None);
        assert!(!control.view_ready().unwrap());

        // Retain precisely what the production handoff retains before retiring
        // the old attachments. Neither the new channels nor an old report may
        // rewrite this scope or restart the original failure budget.
        let media = host.media.take().unwrap();
        let session = host.host.session().unwrap();
        let routes = session.opened.routes;
        let parent = session.opened.binding;
        let previous = media
            .check_recovery_host(
                &session.opened.transport,
                routes,
                parent,
                &host.stream.sender,
                records.inbound,
            )
            .unwrap();
        host.stream
            .sender
            .schedule_recovery(&session.opened.transport, &mut host.stream.source, demand)
            .unwrap();
        let mut waiting = Waiting {
            control: &control,
            until,
            routes,
            obsolete: ObsoleteMetadata::new(
                previous,
                parent,
                media.limits(),
                host.presentation.is_some(),
                host.feedback.is_some(),
            )
            .unwrap(),
            repair: records.repair,
            other: &mut other,
        };
        let replacement = media
            .begin_replacement(
                &h,
                &mut session.opened.transport,
                routes,
                parent,
                until,
                Some([
                    fr_wire::attachment::Ticket(90_001),
                    fr_wire::attachment::Ticket(90_002),
                    fr_wire::attachment::Ticket(90_003),
                ]),
                || control.check().is_ok(),
            )
            .unwrap();
        records.valid(&mut waiting, reports_selected);
        records.refused(&mut waiting);
        assert_eq!(waiting.until, until);
        assert_eq!(waiting.obsolete.previous, records.view);
        assert!(!control.view_ready().unwrap());
        assert_eq!(
            waiting.receive(records.inbound, &records.request()),
            Ok(Disposition::Consumed)
        );
        assert_eq!(waiting.until, until);
        // A malformed negotiated report and a valid unselected report both
        // reach the real session's terminal handler, not callback backpressure.
        let mut invalid = records.reply(records.view);
        if reports_selected {
            invalid[fr_wire::HEADER_BYTES + fr_wire::decoder::BINDING_BYTES] = 2;
        }
        refused_on_connection(&c, session, &mut viewer, &mut waiting, &invalid, &mut nonce).await;
        drop(replacement);
        assert_eq!(
            (
                host.presentation_reports(),
                host.receiver_feedback_reports()
            ),
            reports_before
        );
        assert_eq!(host.worker_id(), worker);
        assert_eq!(host.stream.sender.cache_usage().bytes, 0);
        host.reap_media(
            &cleanup,
            Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        presenter.abort();
        presenter
            .reap(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
    });
}
