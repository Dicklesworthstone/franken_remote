//! Public desktop ownership with actual XCB and TLS/UDP. The scripted peer below
//! stops at display selection: it is NOT host identity, decoder or authority
//! qualification. Existing daemon/native worker suites cover those separately.
use super::*;
use fr_native::desktop::{
    Configuration, Desktop as ClientDesktop, Error as DesktopError, State, WindowCleanup,
};
use frd::{
    session_startup::{ObserverError, ObserverPolicy},
    worker::Deadline,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn owner(display: &str) -> ClientDesktop {
    ClientDesktop::new(Configuration::new(Path::new("/usr/bin/false"), display, None, 43).unwrap())
}
#[test]
fn unpolled_desktop_open_is_terminal_without_a_window_or_foreign_cancellation() {
    let runtime = support::runtime();
    let original = viewer(&runtime);
    let control = original.control();
    let foreign = viewer(&runtime);
    let mut desktop = owner(":0");
    let opening = desktop
        .open(
            original,
            ObserverPolicy::default(),
            None,
            |_| panic!("unpolled display selection"),
            |_| panic!("unpolled approval"),
        )
        .unwrap();
    assert!(!control.is_stopped());
    drop(opening);
    assert!(control.is_stopped());
    assert!(!foreign.control().is_stopped());
    assert_eq!(desktop.state(), State::Stopped);
    assert!(desktop.window().is_none());
    assert_eq!(desktop.window_cleanup(), WindowCleanup::NotStarted);
    assert!(desktop.worker_id().is_none());
    assert!(matches!(
        desktop.serve(|_| Ok(())),
        Err(DesktopError::NotViewing)
    ));
    assert!(desktop.input().is_none());
    assert!(matches!(
        desktop.serve_interactive(
            1,
            fr_core::input_submission::Capabilities::default(),
            fr_client::input::Policy::default(),
            |_, _| Ok(None),
            |_| {}
        ),
        Err(DesktopError::NotViewing)
    ));
    assert_eq!(desktop.collect_clipboard(), Err(DesktopError::NotViewing));
    let second = viewer(&runtime);
    let second_control = second.control();
    assert!(matches!(
        desktop.open(
            second,
            ObserverPolicy::default(),
            None,
            |_| Ok(None),
            |_| Ok(())
        ),
        Err(DesktopError::AlreadyUsed)
    ));
    assert!(second_control.is_stopped());
    assert!(!foreign.control().is_stopped());
}
#[test]
fn parked_desktop_uses_original_budget_and_keeps_failed_cleanup_explicit() {
    let runtime = support::runtime();
    let session = viewer(&runtime);
    let control = session.control();
    let mut desktop = owner(":0");
    let opening = desktop
        .open(
            session,
            ObserverPolicy {
                timeout: Duration::from_millis(10),
                ..ObserverPolicy::default()
            },
            None,
            |_| panic!("expired selection"),
            |_| panic!("expired approval"),
        )
        .unwrap();
    thread::sleep(Duration::from_millis(20));
    assert!(matches!(
        runtime.block_on(opening),
        Err(DesktopError::Observer(ObserverError::Expired))
    ));
    assert!(control.is_stopped());
    assert_eq!(desktop.state(), State::Stopped);
    let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
    let deadline = Deadline::after(&cleanup, Duration::from_secs(1)).unwrap();
    let result = runtime.block_on(desktop.reap(&cleanup, deadline));
    assert!(matches!(result.media, Ok(None)));
    assert_eq!(
        result.input,
        frd::session_startup::viewer_events::CaptureCleanup::NotStarted
    );
    assert_eq!(result.window, WindowCleanup::NotStarted);
    assert_eq!(
        result.clipboard,
        Ok(frd::native_clipboard::Cleanup::NotStarted)
    );
    assert!(cleanup.checkpoint().is_ok());
}
#[test]
fn invalid_local_policy_and_unpolled_cleanup_never_start_or_reopen_a_desktop() {
    let runtime = support::runtime();
    let session = viewer(&runtime);
    let control = session.control();
    let mut desktop = owner(":0");
    let opening = desktop
        .open(
            session,
            ObserverPolicy {
                network_turn: Duration::ZERO,
                ..ObserverPolicy::default()
            },
            None,
            |_| Ok(None),
            |_| Ok(()),
        )
        .unwrap();
    assert!(matches!(
        runtime.block_on(opening),
        Err(DesktopError::Observer(ObserverError::InvalidConfiguration))
    ));
    assert!(control.is_stopped());
    assert!(desktop.window().is_none());
    let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
    let mut fresh = owner(":0");
    let deadline = Deadline::after(&cleanup, Duration::from_secs(1)).unwrap();
    drop(fresh.reap(&cleanup, deadline));
    assert_eq!(fresh.state(), State::Stopped);
    assert!(cleanup.checkpoint().is_ok());
}
#[test]
fn desktop_configuration_and_debug_do_not_disclose_local_paths_or_session_values() {
    let configuration = Configuration::new(
        Path::new("/private/package-name/fr-media-worker"),
        ":27",
        Some(Path::new("/private/auth-file")),
        0xdecaf,
    )
    .unwrap();
    let printed = format!("{configuration:?}");
    let desktop = ClientDesktop::new(configuration);
    for printed in [printed, format!("{desktop:?}")] {
        for secret in ["package-name", "auth-file", ":27", "decaf"] {
            assert!(!printed.contains(secret));
        }
    }
    assert!(Configuration::new(Path::new("relative-worker"), ":0", None, 1).is_err());
}

use asupersync::cx::Cx;
use fr_core::ids::{DisplayGeometryGeneration, HostBootId, OsSessionId, RemoteSessionId};
use fr_transport::quic::{ControlRoutes, Disposition, QuicRecords, Route};
use fr_wire::{
    attachment, display,
    input::{InputDelivery, InputDirection},
    negotiation::{self, Capability, ControlBinding, Message},
};
fn offered() -> Offer {
    let mut capabilities: Vec<_> = [
        display::CAPABILITY,
        fr_wire::decoder::CAPABILITY,
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
    Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: Role::Observe,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities,
    }
}
async fn receive(q: &mut QuicRecords, cx: &Cx, routes: ControlRoutes) -> Vec<u8> {
    let until = support::clock(cx) + 2_000_000;
    loop {
        assert!(support::clock(cx) < until, "scripted peer deadline");
        let mut body = None;
        q.receive_ready(
            cx,
            || true,
            |_| true,
            |route, bytes| {
                assert_eq!(route, Route::Stream(routes.inbound));
                assert!(bytes.len() <= negotiation::MAX_RECORD);
                if body.is_some() {
                    return Ok(Disposition::Blocked);
                }
                body = Some(bytes.to_vec());
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
        if let Some(body) = body {
            return body;
        }
        q.drive(cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
    }
}
async fn send(q: &mut QuicRecords, cx: &Cx, routes: ControlRoutes, bytes: &[u8]) {
    q.send(
        cx,
        Route::Stream(routes.outbound),
        bytes,
        support::clock(cx) + 2_000_000,
        || true,
    )
    .unwrap();
    while !q.send_staged(routes.outbound).unwrap() {
        q.drive(cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
    }
}
async fn send_message(q: &mut QuicRecords, cx: &Cx, routes: ControlRoutes, message: Message) {
    let mut body = [0; negotiation::MAX_RECORD];
    let len = negotiation::encode(&message, body.len(), &mut body).unwrap();
    send(q, cx, routes, &body[..len]).await;
}
/// Explicit test peer, NOT `Host::from_admitted` and never a production bypass.
async fn startup_peer(
    mut q: QuicRecords,
    cx: &Cx,
    mut routes: ControlRoutes,
    chosen: bool,
) -> (QuicRecords, ControlRoutes, ControlBinding) {
    let hello = receive(&mut q, cx, routes).await;
    assert!(matches!(
        negotiation::decode(&hello, negotiation::MAX_RECORD, 0).unwrap(),
        Message::ClientHello(_)
    ));
    send_message(&mut q, cx, routes, Message::HostCapabilities(offered())).await;
    let selected = receive(&mut q, cx, routes).await;
    assert!(matches!(
        negotiation::decode(&selected, negotiation::MAX_RECORD, 0).unwrap(),
        Message::SelectedConfiguration(_)
    ));
    let binding = ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(11),
        os_session: OsSessionId::from_raw(12),
        remote_session: RemoteSessionId::from_raw(13),
    };
    send_message(
        &mut q,
        cx,
        routes,
        Message::SessionOpened {
            binding,
            selection: offered().select().unwrap(),
            observation_until_us: support::clock(cx) + 3_000_000,
        },
    )
    .await;
    routes = q
        .bind_control(cx, routes, binding.id, negotiation::MAX_RECORD, || true)
        .unwrap();
    let accepted = receive(&mut q, cx, routes).await;
    assert_eq!(
        negotiation::decode(&accepted, negotiation::MAX_RECORD, binding.id).unwrap(),
        Message::BindingAccepted {
            binding: binding.id
        }
    );
    let catalog = display::Catalog::new(
        1,
        &[display::Display {
            handle: 9,
            geometry: DisplayGeometryGeneration::INITIAL,
            x: -320,
            y: 0,
            pixel_width: 320,
            pixel_height: 240,
            logical_width: 320,
            logical_height: 240,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation: 0,
        }],
        &ProtocolLimits::ABSOLUTE,
    )
    .unwrap();
    let mut bytes = [0; display::MAX_CATALOG_BYTES];
    let n = display::encode(
        &display::Message::Catalog(catalog),
        binding,
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    send(&mut q, cx, routes, &bytes[..n]).await;
    if chosen {
        let bytes = receive(&mut q, cx, routes).await;
        let choice = display::decode(
            &bytes,
            binding,
            &ProtocolLimits::ABSOLUTE,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        assert_eq!(
            choice,
            display::Message::Select(catalog.selection(9).unwrap())
        );
    }
    (q, routes, binding)
}
async fn select_peer(
    q: QuicRecords,
    cx: &Cx,
    routes: ControlRoutes,
    stop: frd::session_startup::StreamingViewerControl,
    chosen: bool,
) {
    let (mut q, _, _) = Box::pin(startup_peer(q, cx, routes, chosen)).await;
    // No media channel or false codec/presentation completion. The ORIGINAL
    // desktop's budget must fence the local window while awaiting those stages.
    while !stop.is_stopped() {
        if q.drive(cx, Duration::from_millis(2), || true)
            .await
            .is_err()
        {
            break;
        }
    }
}
#[test]
fn approved_choice_creates_one_native_owner_and_expiry_retains_its_cleanup() {
    let native = Desktop::start();
    for chosen in [false, true] {
        let runtime = support::runtime();
        let c = runtime.request_cx_with_budget(Budget::INFINITE);
        let h = runtime.request_cx_with_budget(Budget::INFINITE);
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let mut desktop = owner(&native.display);
        runtime.block_on(async {
            let (client, host) = support::native_pair(&c, "localhost", ALPN).await;
            let viewer = Viewer::new(
                c.clone(),
                client.unwrap(),
                offered(),
                Policy::default(),
                Duration::from_secs(2),
            )
            .unwrap();
            let stop = viewer.control();
            let (q, routes) = QuicRecords::bootstrap(host.unwrap(), &h, Policy::default()).unwrap();
            let opening = desktop
                .open(
                    viewer,
                    ObserverPolicy {
                        timeout: Duration::from_millis(700),
                        ..ObserverPolicy::default()
                    },
                    None,
                    move |_| {
                        counted.fetch_add(1, Ordering::Relaxed);
                        Ok(chosen.then_some(9))
                    },
                    |_| Ok(()),
                )
                .unwrap();
            let (opened, ()) = Box::pin(support::both(
                opening,
                select_peer(q, &h, routes, stop.clone(), chosen),
            ))
            .await;
            assert!(opened.is_err());
            assert!(stop.is_stopped());
        });
        assert!(calls.load(Ordering::Relaxed) > 0);
        assert_eq!(desktop.state(), State::Stopped);
        assert_eq!(desktop.window().is_some(), chosen);
        assert!(desktop.worker_id().is_none());
        if chosen {
            wait(|| matches!(desktop.window_cleanup(), WindowCleanup::Complete(_)));
        } else {
            assert_eq!(desktop.window_cleanup(), WindowCleanup::NotStarted);
        }
        assert!(h.checkpoint().is_ok());
    }
}

#[test]
fn a_panicking_ui_fences_before_the_caller_drops_the_retained_open_future() {
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    let (client, host) = runtime.block_on(support::native_pair(&c, "localhost", ALPN));
    let viewer = Viewer::new(
        c.clone(),
        client.unwrap(),
        offered(),
        Policy::default(),
        Duration::from_secs(2),
    )
    .unwrap();
    let stop = viewer.control();
    let (q, routes) = QuicRecords::bootstrap(host.unwrap(), &h, Policy::default()).unwrap();
    let mut desktop = owner(":0");
    let mut opening = Box::pin(
        desktop
            .open(
                viewer,
                ObserverPolicy::default(),
                None,
                |_| panic!("explicit UI panic fixture"),
                |_| Ok(()),
            )
            .unwrap(),
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(Box::pin(support::both(
            opening.as_mut(),
            select_peer(q, &h, routes, stop.clone(), false),
        )))
    }));
    assert!(result.is_err());
    // The failed future is still retained here. Waiting for its Drop would
    // leave authority/native input live when a containing UI catches the panic.
    assert!(
        stop.is_stopped(),
        "retained panicked future left the original session live"
    );
    drop(opening);
    assert_eq!(desktop.state(), State::Stopped);
    assert!(desktop.window().is_none());
    assert!(h.checkpoint().is_ok());
}

#[path = "desktop/reconnect.rs"]
mod reconnect;
