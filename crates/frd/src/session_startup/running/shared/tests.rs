//! Actual TLS/UDP, session negotiation/renewal, shared ownership and source IPC.
//! Tailnet identity, encoded bytes and decoder completions are explicit fixtures.
use super::*;
use crate::session_startup::running::controlled::tests::attach;
use crate::{
    media::{CaptureSource, decoder_startup, shared_publisher::Publisher},
    media_quic::NegotiatedMedia,
    session_startup::{Configuration, Host, Peer, Viewer, ViewerSession, tests::support},
    worker::{Deadline, Launch},
};
use asupersync::{cx::Cx, runtime::Runtime, types::Budget};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    limits::ProtocolLimits,
    time::HostDuration,
};
use fr_media::{
    delivery::{
        BudgetUsage, MediaBudget, ReceivePipeline, ReceivePolicy, SendPolicy, SharedFramePool,
    },
    worker::{Backend, Configuration as Codec, Role as WorkerRole},
};
use fr_transport::quic::{self, Messages, Policy, StreamRoute};
use fr_wire::{
    attachment::{self, MediaRole},
    decoder,
    input::{InputDelivery, InputDirection},
    negotiation::{Capability, ControlBinding, Offer},
};
use std::{
    fmt::Write,
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

#[allow(clippy::unnecessary_wraps)]
fn block(_: Route, _: &[u8]) -> Result<Disposition, ()> {
    Ok(Disposition::Blocked)
}
fn nonce(n: &mut u128) -> Result<u128, ()> {
    *n = n.checked_add(1).ok_or(())?;
    Ok(*n)
}
fn codec() -> Codec {
    Codec {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
async fn source(control: &ObservationControl) -> CaptureSource {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut payload = String::new();
    for nal in [
        "40010c01ffff01600000030090000003000003003cba0240",
        "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04",
        "4401c0718112",
        "2801ade06702f86753c11ead2f1f6a69",
    ] {
        write!(payload, "{:08x}{nal}", nal.len() / 2).unwrap();
    }
    let script = include_str!("../../../../tests/support/recovery_worker_fixture.py")
        .replace("@MODE@", "healthy")
        .replace(
            "b\"test-only-unit\"",
            &format!("bytes.fromhex('{payload}')"),
        );
    let path = std::env::temp_dir().join(format!(
        "fr-shared-session-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    CaptureSource::start(
        control,
        Launch::new(&path, ":0", None, WorkerRole::Capture, 45).unwrap(),
        codec(),
    )
    .await
    .unwrap()
}
fn source_control(rt: &Runtime) -> ObservationControl {
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let mut a = SessionAuthority::new(
        RemoteSessionId::from_raw(1),
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(5_000_000),
            ticket_lifetime: HostDuration::from_micros(1_000_000),
        },
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(crate::media::host_now(&cx).unwrap())
        .unwrap();
    ObservationControl::new(cx, a).unwrap()
}
struct Member {
    h: Cx,
    c: Cx,
    session: Option<HostSession>,
    shared: Option<SharedHost>,
    viewer: ViewerSession,
    control: ObservationControl,
    host_media: Option<NegotiatedMedia>,
    media: NegotiatedMedia,
    reply: StreamRoute,
    receiver: ReceivePipeline,
    configuration: Option<Vec<u8>>,
    configured: bool,
    first: Option<u64>,
    acknowledged: bool,
    frames: Vec<u64>,
    nonce: u128,
}
async fn sessions(rt: &Runtime, id: u128, role: Role) -> (Cx, Cx, HostSession, ViewerSession) {
    let c = rt.request_cx_with_budget(Budget::INFINITE);
    let h = rt.request_cx_with_budget(Budget::INFINITE);
    let mut caps: Vec<_> = [
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
    caps.sort_by(|host_result, viewer_result| host_result.name.cmp(&viewer_result.name));
    let cfg = Configuration {
        offer: Offer {
            versions: vec![0],
            profile: 1,
            profile_version: 0,
            role,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities: caps,
        },
        binding: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(id),
        },
        require_approval: false,
        startup_timeout: Duration::from_secs(2),
        authority: AuthorityPolicy::plan_defaults(),
        transport: Policy {
            critical_send_records: 1,
            ..Policy::default()
        },
    };
    let (client, native) = support::native_pair(&c, "localhost", quic::ALPN).await;
    let identity = Peer::Fixture {
        alive: Arc::new(AtomicBool::new(true)),
        until: now(&h).unwrap() + 30_000_000,
        control: true,
    };
    let mut viewer = Viewer::new(
        c.clone(),
        client.unwrap(),
        cfg.offer.clone(),
        cfg.transport,
        Duration::from_secs(2),
    )
    .unwrap();
    let mut host = Host::start(h.clone(), native.unwrap(), identity, cfg).unwrap();
    while !host.is_complete() || !viewer.is_complete() {
        let (host_result, viewer_result) = Box::pin(support::both(
            host.drive(Duration::from_millis(1)),
            viewer.drive(Duration::from_millis(1)),
        ))
        .await;
        host_result.unwrap();
        viewer_result.unwrap();
    }
    (
        c,
        h,
        host.finish().unwrap().into_running().unwrap(),
        viewer.finish().unwrap(),
    )
}
async fn peer(rt: &Runtime, id: u128, role: Role) -> Member {
    let (c, h, mut session, mut viewer) = Box::pin(sessions(rt, id, role)).await;
    let (hc, vc) = Box::pin(attach(
        &mut session,
        &mut viewer,
        &c,
        &h,
        MediaRole::Configuration,
        18,
    ))
    .await;
    let (hr, vr) = Box::pin(attach(
        &mut session,
        &mut viewer,
        &c,
        &h,
        MediaRole::Recovery,
        19,
    ))
    .await;
    let (hv, vv) = Box::pin(attach(
        &mut session,
        &mut viewer,
        &c,
        &h,
        MediaRole::Video,
        20,
    ))
    .await;
    let selection = session.selection().clone();
    let host_media =
        NegotiatedMedia::new(session.io().unwrap().0, &selection, &hc, &hr, &hv).unwrap();
    let media = NegotiatedMedia::new(viewer.io().unwrap().0, &selection, &vc, &vr, &vv).unwrap();
    let reply = vc.completed_on(viewer.io().unwrap().0).unwrap().outbound;
    let config = media
        .receiver_config(viewer.io().unwrap().0, ReceivePolicy::default())
        .unwrap();
    let receiver =
        ReceivePipeline::new(config, MediaBudget::new(config.limits.protocol()).unwrap()).unwrap();
    let control = session.observation().unwrap();
    Member {
        h,
        c,
        session: Some(session),
        shared: None,
        viewer,
        control,
        host_media: Some(host_media),
        media,
        reply,
        receiver,
        configuration: None,
        configured: false,
        first: None,
        acknowledged: false,
        frames: vec![],
        nonce: 10000,
    }
}
struct Group {
    publisher: Publisher,
    owner: ObservationControl,
    peers: Vec<Member>,
}
async fn group(rt: &Runtime, count: usize) -> Box<Group> {
    let mut peers = Vec::new();
    for i in 0..count {
        peers.push(Box::pin(peer(rt, 13 + u128::try_from(i).unwrap(), Role::Observe)).await);
    }
    let owner = source_control(rt);
    let mut src = source(&owner).await;
    let pool = SharedFramePool::new(ProtocolLimits::ABSOLUTE, 32 * 1024 * 1024, 8).unwrap();
    let initial = src
        .prepare_shared_capture(&owner, &pool)
        .unwrap()
        .capture_if_changed(true)
        .await
        .unwrap();
    let mut publisher = Publisher::new(src, owner.clone(), pool, &initial).unwrap();
    for p in &mut peers {
        let mut session = p.session.take().unwrap();
        let q = session.io().unwrap().0;
        let media = p.host_media.take().unwrap();
        let setup = media.decoder_setup(q, Duration::from_secs(2)).unwrap();
        let host = decoder_startup::Host::new_shared(
            p.control.clone(),
            q,
            setup,
            codec(),
            initial.clone(),
        )
        .unwrap();
        let sender = media
            .sender(q, p.control.clone(), SendPolicy::default())
            .unwrap();
        let sub = publisher.admit_pending(host, sender, media, q).unwrap();
        p.shared = Some(session.into_shared(sub).unwrap());
    }
    Box::new(Group {
        publisher,
        owner,
        peers,
    })
}
impl Member {
    fn prepare_reply(&mut self) {
        let message = if self.configuration.is_some() && !self.configured {
            Some(decoder::Message::Configured)
        } else if !self.acknowledged {
            self.first.map(|frame| decoder::Message::FirstDecoded {
                frame,
                decoder_micros: now(&self.c).unwrap(),
            })
        } else {
            None
        };
        let Some(message) = message else {
            return;
        };
        let mut bytes = [0; 512];
        let n = decoder::encode(
            message,
            self.media.binding(),
            self.media.limits().protocol(),
            &mut bytes,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        match self.viewer.io().unwrap().0.send(
            &self.c,
            Route::Stream(self.reply),
            &bytes[..n],
            now(&self.c).unwrap() + 500_000,
            || true,
        ) {
            Ok(()) => {
                if message == decoder::Message::Configured {
                    self.configured = true;
                    self.receiver
                        .decoder_configured(now(&self.c).unwrap())
                        .unwrap();
                } else {
                    self.acknowledged = true;
                }
            }
            Err(quic::Error::Backpressure) => {}
            other => panic!("reply: {other:?}"),
        }
    }
    async fn turn(&mut self) -> Result<(), Error> {
        self.prepare_reply();
        let cfg = &mut self.configuration;
        let (a, b) = Box::pin(support::both(
            self.shared.as_mut().unwrap().drive(
                Duration::from_millis(1),
                || nonce(&mut self.nonce),
                block,
            ),
            self.viewer.drive(Duration::from_millis(1), |route, bytes| {
                if matches!(route,Route::Stream(r) if r.messages==Messages::Exact(0x30)) {
                    assert!(cfg.is_none());
                    *cfg = Some(bytes.to_vec());
                    Ok(Disposition::Consumed)
                } else {
                    Ok(Disposition::Blocked)
                }
            }),
        ))
        .await;
        a?;
        b?;
        if let Some(bytes) = &self.configuration {
            assert!(matches!(
                decoder::decode(
                    bytes,
                    self.media.binding(),
                    self.media.limits().protocol(),
                    InputDirection::HostToViewer,
                    InputDelivery::Reliable
                )
                .unwrap(),
                decoder::Message::Configuration(_)
            ));
        }
        self.media
            .receive_ready(
                &self.c,
                self.viewer.io().unwrap().0,
                || true,
                |channel, bytes| {
                    self.receiver
                        .receive(channel, bytes, now(&self.c).unwrap())
                        .unwrap();
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        while let Some(picture) = self.receiver.take_decodable(now(&self.c).unwrap()).unwrap() {
            let frame = self
                .receiver
                .complete_decode(&picture, now(&self.c).unwrap())
                .unwrap()
                .descriptor()
                .frame;
            self.first.get_or_insert(frame);
            self.frames.push(frame);
        }
        // These peers share one fixture task with Publisher::serve. Match the
        // production SharedHost::serve cooperative turn boundary so a constantly
        // ready socket cannot monopolize this manually joined parent future.
        asupersync::runtime::yield_now().await;
        Ok(())
    }
    fn complete(&mut self) -> bool {
        self.shared.as_mut().unwrap().startup_complete().unwrap()
    }
}
async fn ready(g: &mut Group) {
    let until = now(&g.peers[0].h).unwrap() + 1_500_000;
    while g.peers.iter_mut().any(|p| !p.complete()) {
        assert!(now(&g.peers[0].h).unwrap() < until);
        for p in &mut g.peers {
            p.turn().await.unwrap();
        }
    }
}
async fn cleanup(g: &mut Group, cx: &Cx) {
    for p in &mut g.peers {
        if let Some(host) = &mut p.shared {
            host.close();
        }
        p.viewer.close();
    }
    g.publisher
        .reap(cx, Deadline::after(cx, Duration::from_secs(1)).unwrap())
        .await
        .unwrap();
    assert_eq!(g.publisher.physical_usage(), BudgetUsage::default());
}

#[test]
fn original_sessions_renew_while_shared_capture_runs_and_one_viewer_leaves() {
    let rt = support::runtime();
    let cleanup_cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group(&rt, 2)).await;
        ready(&mut g).await;
        let original = g.publisher.worker_id();
        let start = now(&g.peers[0].h).unwrap();
        let mut reports = 0;
        let source = g
            .publisher
            .serve(Duration::from_millis(50), |_| reports += 1);
        let network = async {
            let (first, second) = g.peers.split_at_mut(1);
            let first = &mut first[0];
            let second = &mut second[0];
            while now(&second.h).unwrap() < start + 3_400_000 {
                let (first_result, second_result) =
                    Box::pin(support::both(first.turn(), second.turn())).await;
                first_result.unwrap();
                second_result.unwrap();
            }
            assert!(
                first
                    .shared
                    .as_ref()
                    .unwrap()
                    .renewed_until()
                    .unwrap()
                    .as_micros()
                    > start + 3_000_000
            );
            assert!(
                second
                    .shared
                    .as_ref()
                    .unwrap()
                    .renewed_until()
                    .unwrap()
                    .as_micros()
                    > start + 3_000_000
            );
            let before = second
                .shared
                .as_ref()
                .unwrap()
                .statistics()
                .admitted_records;
            first.shared.as_mut().unwrap().close();
            assert!(first.control.check().is_err());
            while now(&second.h).unwrap() < start + 3_650_000 {
                second.turn().await.unwrap();
            }
            assert!(
                second
                    .shared
                    .as_ref()
                    .unwrap()
                    .statistics()
                    .admitted_records
                    > before
            );
            assert!(second.control.check().is_ok());
            assert_eq!(second.frames, [0]); // Genuine static source observations, no dummy video.
            second.shared.as_mut().unwrap().close();
        };
        let (result, ()) = Box::pin(support::both(source, network)).await;
        assert_eq!(result, Err(crate::media::shared_publisher::Error::Closed));
        assert!(reports > 10);
        assert_eq!(g.publisher.worker_id(), original);
        assert!(g.owner.check().is_err());
        cleanup(&mut g, &cleanup_cx).await;
    });
}

#[test]
fn unpolled_session_turn_retires_only_its_original_member() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group(&rt, 2)).await;
        drop(g.peers[0].shared.as_mut().unwrap().drive(
            Duration::from_millis(1),
            || Ok(123),
            block,
        ));
        assert!(g.peers[0].control.check().is_err());
        assert!(g.peers[1].control.check().is_ok());
        assert_eq!(g.publisher.tick().unwrap(), 1);
        let until = now(&g.peers[1].h).unwrap() + 1_000_000;
        while !g.peers[1].complete() {
            assert!(now(&g.peers[1].h).unwrap() < until);
            g.peers[1].turn().await.unwrap();
        }
        assert!(g.owner.check().is_ok());
        cleanup(&mut g, &cx).await;
    });
}

#[test]
fn source_revocation_blocks_session_udp_and_releases_all_shared_references() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group(&rt, 2)).await;
        ready(&mut g).await;
        g.publisher.capture_next().await.unwrap();
        let before = g.peers[0].shared.as_ref().unwrap().statistics();
        g.owner.revoke();
        assert!(g.peers[0].turn().await.is_err());
        assert!(g.peers[1].turn().await.is_err());
        assert_eq!(g.peers[0].shared.as_ref().unwrap().statistics(), before);
        assert!(g.peers.iter().all(|p| p.control.check().is_err()));
        assert_eq!(g.publisher.physical_usage(), BudgetUsage::default());
        cleanup(&mut g, &cx).await;
    });
}

#[test]
fn a_foreign_session_cannot_borrow_a_shared_member_with_equal_numeric_ids() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group(&rt, 1)).await;
        let mut other = Box::pin(peer(&rt, 13, Role::Observe)).await;
        let original = g.peers[0].shared.as_mut().unwrap();
        let mut member = original.subscriber.take().unwrap();
        let foreign = other.session.take().unwrap();
        assert!(matches!(
            member.bind_session(
                &foreign.opened.transport,
                &foreign.opened.control,
                foreign.binding()
            ),
            Err(crate::media::shared_publisher::Error::ForeignConnection)
        ));
        // Identity inspection itself is non-mutating; the original session is live.
        assert!(original.session.opened.control.check().is_ok());
        original.subscriber = Some(member);
        assert!(
            foreign
                .into_shared(original.subscriber.take().unwrap())
                .is_err()
        );
        assert!(g.peers[0].control.check().is_err());
        assert!(g.owner.check().is_err());
        cleanup(&mut g, &cx).await;
    });
}

#[test]
fn invalid_turn_budget_and_unpolled_continuous_service_are_terminal() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group(&rt, 2)).await;
        assert_eq!(
            g.peers[0]
                .shared
                .as_mut()
                .unwrap()
                .drive(Duration::from_millis(101), || Ok(999), block)
                .await,
            Err(Error::InvalidConfiguration)
        );
        assert!(g.peers[0].control.check().is_err());
        assert!(g.owner.check().is_ok());
        drop(g.peers[1].shared.as_mut().unwrap().serve(|| Ok(999), block));
        assert!(g.peers[1].control.check().is_err());
        assert!(g.owner.check().is_err());
        cleanup(&mut g, &cx).await;
    });
}
