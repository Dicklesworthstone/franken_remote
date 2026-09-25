//! The shipped client must expose the actual content-free host refusal, not a
//! generic transport failure. Real CLI/TLS/UDP/Host; `LocalAPI` and ingress fixtures.
use super::*;
use frd::session_startup::Error as SessionError;
use std::cell::Cell;

fn refused_inspection(deny_approval: bool) {
    let fr = shipped_client();
    let api = fixture::Api::new();
    let client_api = ClientApi::new();
    let roots = fixture::pki().join("ca.pem");
    let runtime = network::runtime();
    let host_cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let clock = runtime.request_cx_with_budget(Budget::INFINITE);
    let approvals = Cell::new(0);
    let calls = Cell::new(0);
    let output = runtime.block_on(async {
        let identity = api.identity(&host_cx).await;
        let mut server = Server::new(api.client.clone(), identity);
        let socket = Listener::bind(&host_cx, address(), native_accept::Configuration::default())
            .await
            .unwrap();
        let mut request = fixture::request();
        if deny_approval {
            request.session.offer = fr_client::native::observation_offer();
        }
        let path = client_api.path.clone();
        let client = thread::spawn(move || run_displays(&fr, &path, &roots));
        let result = timeout(
            clock.now(),
            Duration::from_secs(8),
            server.run_on_protected_listener(
                &host_cx,
                socket,
                request,
                fixture::boundary(address(), Arc::new(AtomicBool::new(true))),
                |host| async {
                    calls.set(calls.get() + 1);
                    assert!(api.whois.load(Ordering::Acquire) > 0);
                    host.open(Duration::from_millis(2), |approval, role| {
                        assert!(
                            deny_approval,
                            "capability refusal precedes any approval prompt"
                        );
                        assert_eq!(role, fr_wire::negotiation::Role::Observe);
                        approvals.set(approvals.get() + 1);
                        approval.decide(false).unwrap();
                        Ok(())
                    })
                    .await
                    .map(|_| panic!("a refused session must not open"))
                },
            ),
        )
        .await
        .unwrap();
        let expected = if deny_approval {
            SessionError::Denied
        } else {
            SessionError::Protocol(fr_wire::negotiation::Error::RequiredCapability)
        };
        assert_eq!(result, Ok(Err(expected)), "retain the original host result");
        assert!(host_cx.is_cancel_requested(), "host authority fenced");
        client.join().unwrap()
    });
    assert_eq!(
        calls.get(),
        1,
        "one attempt, no automatic retry or fallback"
    );
    assert_eq!(approvals.get(), u32::from(deny_approval));
    assert_eq!(output.status.code(), Some(1));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}; stdout {:?}; stderr {:?}",
            output.stdout, output.stderr
        )
    });
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["outcome"], "refused");
    assert_eq!(
        value["error"]["code"],
        if deny_approval {
            "host_local_approval_denied"
        } else {
            "host_required_capability_missing"
        }
    );
    assert_ne!(value["error"]["next_action"].as_str().unwrap(), "");
    assert!(
        value.get("displays").is_none(),
        "refusal is not a successful inventory"
    );
    assert_eq!(
        output.stderr, b"",
        "content-free refusal belongs in the result envelope"
    );
    assert!(
        UdpSocket::bind(address()).is_ok(),
        "original transport retired"
    );
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn shipped_fr_displays_reports_host_capability_refusal_without_approval_or_retry() {
    refused_inspection(false);
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn shipped_fr_displays_reports_local_approval_denial_without_retry() {
    refused_inspection(true);
}
