#![cfg(target_os = "linux")]
//! Real TLS/UDP and completed media attachments. Decode completion is simulated;
//! these tests do not qualify HEVC, native presentation or live-tailnet admission.
#[path = "../../fr-transport/tests/support/mod.rs"]
#[allow(dead_code)]
mod net;
use asupersync::cx::Cx;
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_media::delivery::{MediaBudget, ReceivePipeline, ReceivePolicy};
use fr_transport::quic::*;
use fr_wire::{
    attachment::{self, MediaRole, Ticket},
    decoder::{self, Binding},
    negotiation::{Capability, ControlBinding, Offer, Role, Selection},
    recovery_request as wire,
};
use frd::media_quic::{NegotiatedMedia, replacement::Replacement};
use std::{cell::Cell, time::Duration};
fn parent() -> ControlBinding {
    ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(11),
        os_session: OsSessionId::from_raw(12),
        remote_session: RemoteSessionId::from_raw(13),
    }
}
fn binding(id: u32) -> Binding {
    Binding {
        parent: ControlBinding { id, ..parent() },
        display: 14,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
struct Link {
    c: QuicRecords,
    h: QuicRecords,
    cr: ControlRoutes,
    hr: ControlRoutes,
    selection: Selection,
}
impl Link {
    async fn new(cx: &Cx, enabled: bool) -> Self {
        let (c, h) = net::native_pair(cx, "localhost", ALPN).await;
        let policy = Policy {
            critical_send_records: 1,
            ..Policy::default()
        };
        let (mut c, cr) = QuicRecords::bootstrap(c.unwrap(), cx, policy).unwrap();
        let (mut h, hr) = QuicRecords::bootstrap(h.unwrap(), cx, policy).unwrap();
        let cr = c.bind_control(cx, cr, 7, 4096, || true).unwrap();
        let hr = h.bind_control(cx, hr, 7, 4096, || true).unwrap();
        let mut capabilities: Vec<_> = [
            decoder::CAPABILITY,
            attachment::CAPABILITY,
            attachment::DELIVERY_CAPABILITY,
        ]
        .into_iter()
        .map(|name| Capability {
            name: name.into(),
            version: 1,
            required: true,
        })
        .collect();
        if enabled {
            capabilities.push(Capability {
                name: wire::CAPABILITY.into(),
                version: wire::VERSION,
                required: true,
            });
        }
        capabilities.sort_by(|a, b| a.name.cmp(&b.name));
        let selection = Offer {
            versions: vec![0],
            profile: 1,
            profile_version: 0,
            role: Role::Observe,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities,
        }
        .select()
        .unwrap();
        Self {
            c,
            h,
            cr,
            hr,
            selection,
        }
    }
    async fn drive(&mut self, cx: &Cx) {
        let (a, b) = Box::pin(net::both(
            self.h.drive(cx, Duration::from_millis(1), || true),
            self.c.drive(cx, Duration::from_millis(1), || true),
        ))
        .await;
        a.unwrap();
        b.unwrap();
    }
    async fn attach(&mut self, cx: &Cx, role: MediaRole, id: u32) -> (MediaChannel, MediaChannel) {
        let until = net::clock(cx) + 1_500_000;
        let mut h = self
            .h
            .offer_media_role(
                cx,
                ChannelScope {
                    control: self.hr,
                    parent: parent(),
                    selection: &self.selection,
                },
                ChannelRequest {
                    binding: binding(id),
                    ticket: Ticket(1000 + u128::from(id)),
                    timeout: Duration::from_secs(2),
                },
                role,
                || true,
            )
            .unwrap();
        let mut sent = false;
        let mut c = loop {
            assert!(net::clock(cx) < until);
            if !sent {
                sent = h.transmit(&mut self.h, cx, || true).unwrap();
            }
            self.drive(cx).await;
            let ready = Cell::new(true);
            let mut record = None;
            self.c
                .receive_ready(
                    cx,
                    || true,
                    |r| ready.get() && r == Route::Stream(self.cr.inbound),
                    |_, b| {
                        ready.set(false);
                        record = Some(b.to_vec());
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if let Some(bytes) = record {
                break self
                    .c
                    .accept_media_channel(
                        cx,
                        ChannelScope {
                            control: self.cr,
                            parent: parent(),
                            selection: &self.selection,
                        },
                        &bytes,
                        Duration::from_secs(2),
                        || true,
                    )
                    .unwrap();
            }
        };
        loop {
            assert!(net::clock(cx) < until);
            h.transmit(&mut self.h, cx, || true).unwrap();
            c.transmit(&mut self.c, cx, || true).unwrap();
            self.drive(cx).await;
            h.dispatch(&mut self.h, cx, || true).unwrap();
            c.dispatch(&mut self.c, cx, || true).unwrap();
            let a = h.finish(&mut self.h, cx, || true).unwrap();
            let b = c.finish(&mut self.c, cx, || true).unwrap();
            if a.is_some() && b.is_some() {
                break;
            }
        }
        (h, c)
    }
    async fn media(&mut self, cx: &Cx) -> (NegotiatedMedia, NegotiatedMedia, ReceivePipeline) {
        let (hc, cc) = self.attach(cx, MediaRole::Configuration, 8).await;
        let (hr, cr) = self.attach(cx, MediaRole::Recovery, 9).await;
        let (hv, cv) = self.attach(cx, MediaRole::Video, 10).await;
        let host = NegotiatedMedia::new(&self.h, &self.selection, &hc, &hr, &hv).unwrap();
        let viewer = NegotiatedMedia::new(&self.c, &self.selection, &cc, &cr, &cv).unwrap();
        let cfg = viewer
            .receiver_config(&self.c, ReceivePolicy::default())
            .unwrap();
        let receiver =
            ReceivePipeline::new(cfg, MediaBudget::new(cfg.limits.protocol()).unwrap()).unwrap();
        (host, viewer, receiver)
    }
}
fn tickets() -> [Ticket; 3] {
    [Ticket(8001), Ticket(8002), Ticket(8003)]
}
async fn complete(link: &mut Link, cx: &Cx, host: &mut Replacement, viewer: &mut Replacement) {
    let original = (host.deadline_micros(), viewer.deadline_micros());
    loop {
        assert!(net::clock(cx) < original.0);
        let h = host.advance(cx, &mut link.h, || true).unwrap();
        let c = viewer.advance(cx, &mut link.c, || true).unwrap();
        assert_eq!((host.deadline_micros(), viewer.deadline_micros()), original);
        if h && c {
            break;
        }
        link.drive(cx).await;
    }
}
async fn replace(
    link: &mut Link,
    cx: &Cx,
    host: NegotiatedMedia,
    viewer: NegotiatedMedia,
) -> (NegotiatedMedia, NegotiatedMedia) {
    let until = net::clock(cx) + 1_500_000;
    let mut h = host
        .begin_replacement(
            cx,
            &mut link.h,
            link.hr,
            parent(),
            until,
            Some(tickets()),
            || true,
        )
        .unwrap();
    let mut c = viewer
        .begin_replacement(cx, &mut link.c, link.cr, parent(), until, None, || true)
        .unwrap();
    complete(link, cx, &mut h, &mut c).await;
    (
        h.finish(cx, &mut link.h).unwrap(),
        c.finish(cx, &mut link.c).unwrap(),
    )
}
fn observation(cx: &Cx) -> frd::media::ObservationControl {
    use fr_core::authority::{AuthorityPolicy, SessionAuthority};
    let now = frd::media::host_now(cx).unwrap();
    let mut authority =
        SessionAuthority::new(parent().remote_session, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(now).unwrap();
    frd::media::ObservationControl::new(cx.clone(), authority).unwrap()
}

use fr_media::{
    delivery::SendPolicy,
    worker::{Backend, Configuration, Role as WorkerRole},
};
use frd::{
    media::{CaptureSource, ObservationControl},
    media_quic::{Error, QuicEgress},
    worker::{Deadline, Launch},
};
use std::{
    os::unix::fs::PermissionsExt,
    sync::atomic::{AtomicU64, Ordering},
};

async fn source(control: &ObservationControl) -> CaptureSource {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-handoff-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(
        &path,
        include_str!("support/recovery_worker_fixture.py").replace("@MODE@", "healthy"),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    CaptureSource::start(
        control,
        Launch::new(&path, ":0", None, WorkerRole::Capture, 1).unwrap(),
        Configuration {
            width: 320,
            height: 240,
            fps: 30,
            backend: Backend::SoftwareExplicit,
            bitrate: 2_000_000,
            max_access_unit_bytes: 1024 * 1024,
            generation: CodecConfigurationGeneration::INITIAL,
        },
    )
    .await
    .unwrap()
}
fn request(mut view: Binding) -> [u8; wire::REQUEST_BYTES] {
    view.parent = parent();
    let mut bytes = [0; wire::REQUEST_BYTES];
    wire::encode(
        wire::Request {
            reason: wire::Reason::ReferenceExpired,
            last_useful_frame: None,
        },
        view,
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        fr_wire::input::InputDirection::ViewerToHost,
        fr_wire::input::InputDelivery::Reliable,
    )
    .unwrap();
    bytes
}
async fn report(
    link: &mut Link,
    cx: &Cx,
    media: &NegotiatedMedia,
    sender: &mut QuicEgress,
    source: &mut CaptureSource,
) {
    let bytes = request(media.binding());
    link.c
        .send(
            cx,
            Route::Stream(link.cr.outbound),
            &bytes,
            net::clock(cx) + 1_000_000,
            || true,
        )
        .unwrap();
    let mut seen = false;
    while !seen {
        link.drive(cx).await;
        let mut record = None;
        link.h
            .receive_ready(
                cx,
                || true,
                |r| r == Route::Stream(link.hr.inbound),
                |r, b| {
                    record = Some((r, b.to_vec()));
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if let Some((r, b)) = record {
            assert!(
                media
                    .request_recovery(&link.h, link.hr, parent(), sender, source, r, &b)
                    .unwrap()
            );
            seen = true;
        }
    }
}
async fn stop(source: &mut CaptureSource, cx: &Cx) {
    let deadline = Deadline::after(cx, Duration::from_secs(1)).unwrap();
    let worker = source.worker_mut();
    worker
        .request(cx, fr_media::worker::Kind::Stop, vec![], deadline)
        .await
        .unwrap();
    worker.reap(cx, deadline).await.unwrap();
}
#[test]
fn real_report_fresh_channels_and_same_sender_preserve_chronic_failure_credit() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (hm, cm, _receiver) = link.media(&cx).await;
        let control = observation(&cx);
        let mut source = source(&control).await;
        let worker = source.worker_id();
        let mut sender = hm
            .sender(
                &link.h,
                control.clone(),
                SendPolicy {
                    max_recoveries_per_window: 1,
                    ..SendPolicy::default()
                },
            )
            .unwrap();
        sender
            .enqueue_capture(source.capture_if_changed(&control, true).await.unwrap())
            .unwrap();
        report(&mut link, &cx, &hm, &mut sender, &mut source).await;
        assert_eq!(sender.cache_usage().bytes, 0);
        assert!(!sender.is_closed());
        assert!(sender.pending().is_none());
        let fresh = source.capture_if_changed(&control, false).await.unwrap();
        assert!(fresh.encoded().unwrap().is_idr());
        let (hm, _cm) = replace(&mut link, &cx, hm, cm).await;
        hm.recover_sender(&link.h, &mut sender).unwrap();
        sender.enqueue_capture(fresh).unwrap();
        assert_eq!(source.worker_id(), worker);
        assert!(matches!(
            hm.request_recovery(
                &link.h,
                link.hr,
                parent(),
                &mut sender,
                &mut source,
                Route::Stream(link.hr.inbound),
                &request(hm.binding())
            ),
            Err(Error::Media(frd::media::Error::Send(
                fr_media::delivery::SendError::RecoveryLimitExceeded
            )))
        ));
        assert_eq!(source.next_recovery_deadline(), None);
        assert_eq!(sender.cache_usage().bytes, 0);
        stop(&mut source, &cx).await;
    });
}
#[test]
fn malformed_wrong_route_and_foreign_sender_do_not_retire_healthy_work() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (hm, _, _) = link.media(&cx).await;
        let control = observation(&cx);
        let mut source = source(&control).await;
        let mut sender = hm
            .sender(&link.h, control.clone(), SendPolicy::default())
            .unwrap();
        sender
            .enqueue_capture(source.capture_if_changed(&control, true).await.unwrap())
            .unwrap();
        let charged = sender.cache_usage();
        for (route, bytes) in [
            (Route::Stream(link.hr.inbound), b"bad".as_slice()),
            (
                Route::Stream(link.hr.outbound),
                request(hm.binding()).as_slice(),
            ),
        ] {
            assert!(
                hm.request_recovery(
                    &link.h,
                    link.hr,
                    parent(),
                    &mut sender,
                    &mut source,
                    route,
                    bytes
                )
                .is_err()
            );
            assert_eq!(sender.cache_usage(), charged);
            assert_eq!(source.next_recovery_deadline(), None);
        }
        let foreign = Link::new(&cx, true).await;
        assert!(
            hm.request_recovery(
                &foreign.h,
                foreign.hr,
                parent(),
                &mut sender,
                &mut source,
                Route::Stream(foreign.hr.inbound),
                &request(hm.binding())
            )
            .is_err()
        );
        assert!(!foreign.h.is_closed());
        assert!(!link.h.is_closed());
        assert_eq!(sender.cache_usage(), charged);
        assert!(hm.recover_sender(&link.h, &mut sender).is_err());
        assert_eq!(sender.cache_usage(), charged);
        stop(&mut source, &cx).await;
    });
}
#[test]
fn replacement_cannot_renew_an_expired_senders_deadline() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (hm, cm, _) = link.media(&cx).await;
        let control = observation(&cx);
        let mut source = source(&control).await;
        let mut sender = hm
            .sender(
                &link.h,
                control.clone(),
                SendPolicy {
                    recovery_horizon_micros: 150_000,
                    ..SendPolicy::default()
                },
            )
            .unwrap();
        sender
            .enqueue_capture(source.capture_if_changed(&control, true).await.unwrap())
            .unwrap();
        report(&mut link, &cx, &hm, &mut sender, &mut source).await;
        let original = sender.next_deadline().unwrap().as_micros();
        let (hm, _) = replace(&mut link, &cx, hm, cm).await;
        while net::clock(&cx) < original {
            link.drive(&cx).await;
        }
        assert!(matches!(
            hm.recover_sender(&link.h, &mut sender),
            Err(Error::Media(frd::media::Error::Send(
                fr_media::delivery::SendError::Delivery(
                    fr_media::delivery::DeliveryError::RecoveryExpired
                )
            )))
        ));
        assert!(sender.tick().is_err());
        stop(&mut source, &cx).await;
    });
}
