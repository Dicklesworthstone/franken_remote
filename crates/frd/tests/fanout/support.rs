use super::net;
use asupersync::cx::Cx;
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_media::delivery::{MediaBudget, ReceivePipeline, ReceivePolicy};
use fr_transport::quic::*;
use fr_wire::{
    attachment::{self, MediaRole, Ticket},
    decoder::{self, Binding},
    negotiation::{Capability, ControlBinding, Offer, Role, Selection},
};
use frd::media_quic::NegotiatedMedia;
use std::{cell::Cell, time::Duration};
fn parent(session: u128) -> ControlBinding {
    ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(11),
        os_session: OsSessionId::from_raw(12),
        remote_session: RemoteSessionId::from_raw(session),
    }
}
fn binding(id: u32, session: u128) -> Binding {
    Binding {
        parent: ControlBinding {
            id,
            ..parent(session)
        },
        display: 14,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
pub(super) struct Link {
    session: u128,
    pub(super) c: QuicRecords,
    pub(super) h: QuicRecords,
    pub(super) cr: ControlRoutes,
    pub(super) hr: ControlRoutes,
    selection: Selection,
}
impl Link {
    pub(super) async fn new(cx: &Cx, session: u128) -> Self {
        let (c, h) = net::native_pair(cx, "localhost", ALPN).await;
        let policy = Policy {
            critical_send_records: 1,
            retained_send_records: 2,
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
            session,
            c,
            h,
            cr,
            hr,
            selection,
        }
    }
    pub(super) async fn drive(&mut self, cx: &Cx) {
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
                    parent: parent(self.session),
                    selection: &self.selection,
                },
                ChannelRequest {
                    binding: binding(id, self.session),
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
                            parent: parent(self.session),
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
    pub(super) async fn media(
        &mut self,
        cx: &Cx,
    ) -> (NegotiatedMedia, NegotiatedMedia, ReceivePipeline) {
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
use asupersync::{runtime::Runtime, types::Budget};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    input::*,
    input_submission::{Capabilities, InputSession},
    time::HostDuration,
};
use fr_media::{
    delivery::SendPolicy,
    worker::{Backend, Configuration, Role as WorkerRole},
};
use frd::{
    media::{
        CaptureSource, ObservationControl, Presenter,
        decoder_startup::{Host, Viewer},
    },
    media_egress::Lane,
    media_quic::fanout::ReadySender,
    worker::{Deadline, Launch},
};
use std::{
    os::unix::fs::PermissionsExt,
    sync::atomic::{AtomicU64, Ordering},
};

pub(super) fn gate(rt: &Runtime, session: u128) -> (ObservationControl, InputSession) {
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let mut a = SessionAuthority::new(
        RemoteSessionId::from_raw(session),
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(5_000_000),
            ticket_lifetime: HostDuration::from_micros(4_000_000),
        },
    );
    let now = frd::media::host_now(&cx).unwrap();
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(now).unwrap();
    a.mark_view_ready(now).unwrap();
    a.grant_lease(InputLeaseId::from_raw(1), now).unwrap();
    a.issue_input_ticket(InputLeaseId::from_raw(1), InputTicketId::from_raw(1), now)
        .unwrap();
    let control = ObservationControl::new(cx, a).unwrap();
    let input = control
        .input_session(
            InputCredentials {
                session: RemoteSessionId::from_raw(session),
                lease: InputLeaseId::from_raw(1),
                ticket: InputTicketId::from_raw(1),
                view: InputView {
                    geometry: DisplayGeometryGeneration::INITIAL,
                    viewport: ViewportMappingGeneration::INITIAL,
                    configuration: CodecConfigurationGeneration::INITIAL,
                    recovery: RecoveryGeneration::INITIAL,
                },
            },
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            Capabilities::default(),
        )
        .unwrap();
    (control, input)
}
pub(super) fn config() -> Configuration {
    Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 10_000,
        max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
fn fixture(text: &str) -> std::path::PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-sendset-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}
pub(super) async fn source(control: &ObservationControl) -> CaptureSource {
    let path = fixture(
        include_str!("../../src/session_startup/running/streaming/recovery/capture_fixture.py")
            .replace("if frame==2 and not forced:", "if False:")
            .replace("@MODE@", "healthy")
            .replace("b'synthetic-dependent'", "b'x'*9000")
            .as_str(),
    );
    CaptureSource::start(
        control,
        Launch::new(&path, ":0", None, WorkerRole::Capture, 77).unwrap(),
        config(),
    )
    .await
    .unwrap()
}
pub(super) async fn stop(s: &mut CaptureSource, cx: &Cx) {
    let d = Deadline::after(cx, Duration::from_secs(1)).unwrap();
    let w = s.worker_mut();
    w.request(cx, fr_media::worker::Kind::Stop, vec![], d)
        .await
        .unwrap();
    w.reap(cx, d).await.unwrap();
}
impl Link {
    pub(super) async fn ready(
        &mut self,
        cx: &Cx,
        source: &mut CaptureSource,
        owner: &ObservationControl,
        control: &ObservationControl,
        policy: SendPolicy,
    ) -> (ReadySender, NegotiatedMedia, Presenter, ReceivePipeline) {
        let (hm, cm, _) = self.media(cx).await;
        let mut sender = hm.sender(&self.h, control.clone(), policy).unwrap();
        let update = source.capture_if_changed(owner, true).await.unwrap();
        let mut host = Host::new(
            control.clone(),
            &self.h,
            hm.decoder_setup(&self.h, Duration::from_secs(2)).unwrap(),
            config(),
            update,
        )
        .unwrap();
        let until = net::clock(cx) + 2_000_000;
        while !host.transmit(&mut self.h).unwrap() {
            self.drive(cx).await;
        }
        let bytes = loop {
            assert!(net::clock(cx) < until);
            self.drive(cx).await;
            let mut record = None;
            self.c
                .receive_ready(
                    cx,
                    || true,
                    |r| matches!(r,Route::Stream(s) if s.messages==Messages::Exact(0x30)),
                    |_, b| {
                        record = Some(b.to_vec());
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if let Some(b) = record {
                break b;
            }
        };
        let path = fixture(include_str!(
            "../../src/media/presentation/decoder_fixture.py"
        ));
        let mut viewer = Viewer::start(
            cx.clone(),
            &self.c,
            cm.decoder_setup(&self.c, Duration::from_secs(2)).unwrap(),
            &bytes,
            Launch::new(&path, ":0", None, WorkerRole::Present, 33).unwrap(),
            cm.receiver_config(&self.c, ReceivePolicy::default())
                .unwrap(),
        )
        .await
        .unwrap();
        while !viewer.transmit(&mut self.c).unwrap() {
            self.drive(cx).await;
        }
        let mut enqueued = false;
        let mut decoded = false;
        loop {
            assert!(net::clock(cx) < until);
            self.drive(cx).await;
            host.dispatch(&mut self.h).unwrap();
            if !enqueued && let Some(update) = host.take_recovery().unwrap() {
                sender.enqueue_capture(update).unwrap();
                enqueued = true;
            }
            sender.transmit(cx, &mut self.h, Lane::Original).unwrap();
            self.drive(cx).await;
            if !decoded {
                cm.receive_ready(
                    cx,
                    &mut self.c,
                    || true,
                    |channel, b| {
                        viewer.receive_media(channel, b).unwrap();
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
                if viewer.present_first().await.unwrap().is_some() {
                    decoded = true;
                }
            }
            if decoded && !viewer.is_complete() {
                viewer.transmit(&mut self.c).unwrap();
            }
            host.dispatch(&mut self.h).unwrap();
            if viewer.is_complete() && host.is_complete() {
                break;
            }
        }
        // Flush remaining ACK/control bytes before isolating native send credit.
        for _ in 0..4 {
            self.drive(cx).await;
        }
        let (presenter, receiver) = viewer.finish().unwrap();
        let ready = ReadySender::new(host, source, sender, &self.h).unwrap();
        (ready, cm, presenter, receiver)
    }
}
pub(super) fn input_live(cx: &Cx, i: &InputSession) -> bool {
    i.monitor()
        .authorize_ticket(
            InputTicketId::from_raw(1),
            frd::media::host_now(cx).unwrap(),
        )
        .is_ok()
}
