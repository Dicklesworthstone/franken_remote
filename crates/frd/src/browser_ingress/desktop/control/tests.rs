//! Waiting-consent ownership tests. Transport and authority fixtures are
//! explicit; these do not claim installed-tailnet or physical UI qualification.
use super::*;
use asupersync::{
    io::{AsyncWriteExt, duplex},
    runtime::RuntimeBuilder,
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::RemoteSessionId,
};
use std::sync::atomic::AtomicUsize;

fn authorized(cx: &Cx) -> Authorized {
    let mut authority = SessionAuthority::new(
        RemoteSessionId::from_raw(71),
        AuthorityPolicy::plan_defaults(),
    );
    authority.mark_capabilities_checked().unwrap();
    authority
        .authorize_observation(host_now(cx).unwrap())
        .unwrap();
    Authorized {
        control: ObservationControl::new(cx.clone(), authority).unwrap(),
        launch: Launch::new(
            std::path::Path::new("/unused/browser-capture"),
            ":0",
            None,
            fr_media::worker::Role::Capture,
            1,
        )
        .unwrap(),
        select: Box::new(|_, _| Err(())),
        cleanup: cx.clone(),
        supervise: Box::pin(std::future::pending()),
    }
}

#[test]
fn close_or_early_application_bytes_prevent_a_pending_factory_poll() {
    for send_bytes in [false, true] {
        let runtime = RuntimeBuilder::current_thread()
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            let (mut server, mut peer) = duplex(32);
            if send_bytes {
                peer.write_all(&[1]).await.unwrap();
            }
            drop(peer);
            let calls = AtomicUsize::new(0);
            let result = authorize_guarded(&mut server, || Ok(()), async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(Reason::LocalApprovalDenied)
            })
            .await;
            assert!(result.is_err());
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        });
    }
}

struct ClosingRead(Arc<AtomicBool>);
impl AsyncRead for ClosingRead {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.0.load(Ordering::Acquire) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

#[test]
fn disconnect_or_revocation_during_positive_factory_poll_retires_its_authority() {
    for disconnect in [false, true] {
        let runtime = RuntimeBuilder::current_thread()
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            let cx = Cx::current().unwrap();
            let value = authorized(&cx);
            let original = value.control.clone();
            let closed = Arc::new(AtomicBool::new(false));
            let valid = Arc::new(AtomicBool::new(true));
            let mut stream = ClosingRead(closed.clone());
            let result = authorize_guarded(
                &mut stream,
                || {
                    if valid.load(Ordering::Acquire) {
                        Ok(())
                    } else {
                        Err(failed())
                    }
                },
                async {
                    if disconnect {
                        closed.store(true, Ordering::Release);
                    } else {
                        valid.store(false, Ordering::Release);
                    }
                    Ok(value)
                },
            )
            .await;
            assert!(result.is_err());
            assert!(
                original.check().is_err(),
                "a delivered yes cannot survive a simultaneous terminal condition"
            );
        });
    }
}

#[test]
fn connected_original_can_return_a_positive_factory_result_without_input() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let cx = Cx::current().unwrap();
        let value = authorized(&cx);
        let original = value.control.clone();
        let mut stream = ClosingRead(Arc::new(AtomicBool::new(false)));
        let result = authorize_guarded(&mut stream, || Ok(()), async { Ok(value) }).await;
        assert!(matches!(result, Ok(Ok(_))));
        assert!(original.check().is_ok());
        original.revoke();
    });
}
