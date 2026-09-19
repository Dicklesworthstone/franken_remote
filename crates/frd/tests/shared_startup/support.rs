//! Actual independent TLS/UDP peers and completed role-specific attachments.
#[path = "../../../fr-transport/tests/support/mod.rs"]
#[allow(dead_code)]
mod net;
use asupersync::cx::Cx;
use fr_core::{ids::*, limits::ProtocolLimits};
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
fn binding(session: u128, id: u32) -> Binding {
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
    cr: ControlRoutes,
    hr: ControlRoutes,
    pub(super) selection: Selection,
}
impl Link {
    pub(super) async fn new(cx: &Cx, session: u128) -> Self {
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
                    binding: binding(self.session, id),
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
    pub(super) async fn media(&mut self, cx: &Cx) -> Media {
        let (hc, cc) = self.attach(cx, MediaRole::Configuration, 8).await;
        let (hr, cr) = self.attach(cx, MediaRole::Recovery, 9).await;
        let (hv, cv) = self.attach(cx, MediaRole::Video, 10).await;
        let host = NegotiatedMedia::new(&self.h, &self.selection, &hc, &hr, &hv).unwrap();
        let viewer = NegotiatedMedia::new(&self.c, &self.selection, &cc, &cr, &cv).unwrap();
        Media {
            host,
            viewer,
            reply: cc.completed_on(&self.c).unwrap().outbound,
        }
    }
}

pub(super) struct Media {
    pub(super) host: NegotiatedMedia,
    pub(super) viewer: NegotiatedMedia,
    pub(super) reply: StreamRoute,
}

use asupersync::{runtime::Runtime, types::Budget};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    time::HostDuration,
};
use fr_media::{
    delivery::SharedFramePool,
    worker::{Backend, Configuration, Kind, Role as WorkerRole},
};
use frd::{
    media::{CaptureSource, ObservationControl, host_now},
    worker::{Deadline, Launch},
};
use std::{
    fmt::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

pub(super) fn runtime() -> Runtime {
    net::runtime()
}
pub(super) fn clock(cx: &Cx) -> u64 {
    net::clock(cx)
}
pub(super) fn gate(rt: &Runtime, session: u128) -> ObservationControl {
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let mut a = SessionAuthority::new(
        RemoteSessionId::from_raw(session),
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(5_000_000),
            ticket_lifetime: HostDuration::from_micros(1_000_000),
        },
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(host_now(&cx).unwrap()).unwrap();
    ObservationControl::new(cx, a).unwrap()
}
pub(super) fn configuration() -> Configuration {
    Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
pub(super) fn pool() -> SharedFramePool {
    SharedFramePool::new(ProtocolLimits::ABSOLUTE, 32 * 1024 * 1024, 8).unwrap()
}
fn fixture(script: &str, prefix: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-shared-startup-{prefix}-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}
pub(super) async fn source(control: &ObservationControl, valid: bool) -> CaptureSource {
    source_variant(control, valid, false, false).await
}
/// Synthetic changed-frame and pending-IPC modes for shared publisher tests.
/// Native codec output is still canned, never a hardware qualification claim.
pub(super) async fn source_variant(
    control: &ObservationControl,
    valid: bool,
    changing: bool,
    delayed: bool,
) -> CaptureSource {
    // Canned parameter sets and a slice for real bounded host parsing, not a
    // live HEVC encoder. Subsequent decoder completion is explicitly synthetic.
    let mut au = Vec::new();
    for nal in [
        "40010c01ffff01600000030090000003000003003cba0240",
        "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04",
        "4401c0718112",
        "2801ade06702f86753c11ead2f1f6a69",
    ] {
        au.extend_from_slice(&u32::try_from(nal.len() / 2).unwrap().to_be_bytes());
        for h in nal.as_bytes().as_chunks::<2>().0 {
            au.push(u8::from_str_radix(std::str::from_utf8(h).unwrap(), 16).unwrap());
        }
    }
    let mut payload = String::new();
    for value in au {
        write!(payload, "{value:02x}").unwrap();
    }
    let script = include_str!("../support/recovery_worker_fixture.py").replace("@MODE@", "healthy");
    let script = if valid {
        script.replace(
            "b\"test-only-unit\"",
            &format!("bytes.fromhex('{payload}')"),
        )
    } else {
        script
    };
    let script = if changing {
        script.replace(
            "if kind == 7 and not force and last is not None:",
            "if False:",
        )
    } else {
        script
    };
    let script = if delayed {
        script.replace(
            "frame, observed, force = struct.unpack",
            "time.sleep(0.05)\n    frame, observed, force = struct.unpack",
        )
    } else {
        script
    };
    let image = fixture(&script, "source");
    CaptureSource::start(
        control,
        Launch::new(&image, ":0", None, WorkerRole::Capture, 45).unwrap(),
        configuration(),
    )
    .await
    .unwrap()
}
pub(super) fn decoder() -> Launch {
    let image = fixture(
        include_str!("../../src/media/presentation/decoder_fixture.py"),
        "decoder",
    );
    Launch::new(&image, ":0", None, WorkerRole::Present, 46).unwrap()
}
pub(super) async fn stop(source: &mut CaptureSource, cx: &Cx) {
    let deadline = Deadline::after(cx, Duration::from_secs(1)).unwrap();
    let w = source.worker_mut();
    w.request(cx, Kind::Stop, vec![], deadline).await.unwrap();
    w.reap(cx, deadline).await.unwrap();
}
