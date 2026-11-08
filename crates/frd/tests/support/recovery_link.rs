//! Identical TLS, attachment and receive-pipeline setup for recovery suites.
use super::net;
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
use frd::media_quic::NegotiatedMedia;
use std::{cell::Cell, time::Duration};

pub(super) fn parent() -> ControlBinding {
    ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(11),
        os_session: OsSessionId::from_raw(12),
        remote_session: RemoteSessionId::from_raw(13),
    }
}
pub(super) fn binding(id: u32) -> Binding {
    Binding {
        parent: ControlBinding { id, ..parent() },
        display: 14,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
pub(super) struct Link {
    pub(super) c: QuicRecords,
    pub(super) h: QuicRecords,
    pub(super) cr: ControlRoutes,
    pub(super) hr: ControlRoutes,
    pub(super) selection: Selection,
}
impl Link {
    pub(super) async fn new(cx: &Cx, enabled: bool) -> Self {
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
    pub(super) async fn drive(&mut self, cx: &Cx) {
        let (a, b) = Box::pin(net::both(
            self.h.drive(cx, Duration::from_millis(1), || true),
            self.c.drive(cx, Duration::from_millis(1), || true),
        ))
        .await;
        a.unwrap();
        b.unwrap();
    }
    pub(super) async fn attach(
        &mut self,
        cx: &Cx,
        role: MediaRole,
        id: u32,
    ) -> (MediaChannel, MediaChannel) {
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
