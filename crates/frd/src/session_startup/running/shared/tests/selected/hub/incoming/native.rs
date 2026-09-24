//! Actual TLS/UDP and valid observation negotiation; no native source is allowed.
use super::super::super::preparation::{agent, choose};
use super::*;
use crate::session_agent::source::desktop::{Error as DesktopError, LocalAction};
async fn together_native<T>(work: impl Future<Output = T>, peer: impl Future<Output = ()>) -> T {
    let mut work = Box::pin(work);
    let mut peer = Box::pin(peer);
    poll_fn(|task| {
        if let Poll::Ready(result) = work.as_mut().poll(task) {
            return Poll::Ready(result);
        }
        assert!(peer.as_mut().poll(task).is_pending());
        Poll::Pending
    })
    .await
}
#[test]
fn negotiated_observation_without_media_profile_never_launches_native_work() {
    let rt = support::runtime();
    rt.block_on(async {
        let c = rt.request_cx_with_budget(Budget::INFINITE);
        let h = rt.request_cx_with_budget(Budget::INFINITE);
        let offer = Offer {
            versions: vec![0],
            profile: 1,
            profile_version: 0,
            role: Role::Observe,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities: vec![],
        };
        let policy = Policy::default();
        let (client, native) = support::native_pair(&c, "localhost", quic::ALPN).await;
        let mut viewer = Viewer::new(
            c,
            client.unwrap(),
            offer.clone(),
            policy,
            Duration::from_secs(2),
        )
        .unwrap();
        let host = Host::start(
            h.clone(),
            native.unwrap(),
            Peer::Fixture {
                alive: Arc::new(AtomicBool::new(true)),
                until: now(&h).unwrap() + 3_000_000,
                control: false,
            },
            Configuration {
                offer,
                binding: ControlBinding {
                    id: 7,
                    host_boot: HostBootId::from_raw(11),
                    os_session: OsSessionId::from_raw(12),
                    remote_session: RemoteSessionId::from_raw(13),
                },
                require_approval: false,
                startup_timeout: Duration::from_secs(2),
                authority: AuthorityPolicy::plan_defaults(),
                transport: policy,
            },
        )
        .unwrap();
        let mut agent = agent();
        let work = agent
            .open_native_shared_desktop(
                host,
                || panic!("observation negotiation is not media capability"),
                choose,
                service::Policy::default(),
                entropy(),
                |_, _| panic!("unattended"),
                |_, _| Ok(LocalAction::Continue),
            )
            .unwrap();
        let result = Box::pin(together_native(work, async {
            while !viewer.is_complete() {
                viewer.drive(Duration::from_millis(1)).await.unwrap();
            }
            // The original valid observation handshake really completed. Only
            // the unavailable shared media profile must refuse native startup.
            let mut viewer = viewer.finish().unwrap();
            loop {
                let _ = viewer.drive(Duration::from_millis(1), block).await;
                asupersync::runtime::yield_now().await;
            }
        }))
        .await;
        assert!(matches!(
            result,
            Err(DesktopError::Startup(OpenError::InvalidConfiguration))
        ));
        assert!(h.checkpoint().is_err());
    });
}
