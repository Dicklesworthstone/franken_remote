//! Two real native desktops and the actual authenticated UDP/TLS clipboard lane.
//! Grants/local consent/presentation and identifiers are labeled test fixtures.
#![cfg(all(target_os = "linux", feature = "linux-clipboard"))]
#![forbid(unsafe_code)]
#[path = "clipboard_network/attachment.rs"]
mod attachment;
#[path = "clipboard_network/authority.rs"]
mod authority;
#[path = "clipboard_network/desktop.rs"]
mod desktop;
#[path = "clipboard_network/intercept.rs"]
mod intercept;
#[path = "../../fr-transport/tests/support/mod.rs"]
#[allow(dead_code)]
mod transport_support;
use asupersync::cx::Cx;
use attachment::Link;
use desktop::Desktop;
use fr_client::input::InputClient;
use fr_core::{
    clipboard::{Binding, ClipboardSink, Endpoint, Publication, Stamp},
    input_submission::InputSession,
    limits::ProtocolLimits,
};
use fr_native::clipboard::X11Clipboard;
use fr_transport::quic::{Disposition, Route, clipboard::ClipboardChannel};
use fr_wire::{attachment::MediaRole, clipboard::session::synchronize::Received, control::Granted};
use frd::clipboard_quic::{Bridge, State, WorkerTask};
use std::{
    sync::{Arc, Mutex},
    thread::ThreadId,
    time::{Duration, Instant},
};
use transport_support::{both, clock, runtime};

fn open(display: &str) -> X11Clipboard {
    X11Clipboard::open(display, &ProtocolLimits::ABSOLUTE, true).unwrap()
}
fn copy(app: &mut X11Clipboard, text: &str, sequence: u64) {
    let stamp = Stamp {
        id: u128::from(sequence),
        sequence,
        source: Endpoint::Host,
    };
    app.prepare(text, stamp).unwrap();
    assert_eq!(app.publish(text, stamp), Publication::SubmittedToOs);
}
struct World {
    cx: Cx,
    link: Link,
    bridges: [Bridge; 2],
    tasks: [WorkerTask; 2],
    apps: [X11Clipboard; 2],
    host: InputSession,
    viewer: InputClient,
    grant: Granted,
    serial: u64,
    worker_ids: Arc<Mutex<Vec<ThreadId>>>,
    pauses: [Arc<intercept::Pause>; 2],
    _input: (
        fr_transport::quic::MediaChannel,
        fr_transport::quic::MediaChannel,
    ),
    desktops: [Desktop; 2],
}
impl World {
    async fn new(cap: u32) -> Self {
        let cx = Cx::current().unwrap();
        let desktops = [Desktop::new(), Desktop::new()];
        let apps = [open(&desktops[0].display), open(&desktops[1].display)];
        let mut link = Link::new(&cx).await;
        let input = link.attach(&cx, 9, MediaRole::Input).await;
        link.record_cap(cap);
        let (h, c) = link.attach(&cx, 10, MediaRole::Clipboard).await;
        let g = authority::grant(&cx);
        let host = authority::host(g);
        let mut viewer = authority::viewer(&cx, g);
        let scope = Binding {
            session: g.request.parent.remote_session,
            lease: g.lease,
        };
        let h = ClipboardChannel::new(&link.h, h, scope).unwrap();
        let c = ClipboardChannel::new(&link.c, c, scope).unwrap();
        let (host_bridge, host_seed) = Bridge::host(cx.clone(), &link.h, h, &host, true).unwrap();
        let (viewer_bridge, viewer_seed) =
            Bridge::controller(cx.clone(), &link.c, c, &mut viewer, true).unwrap();
        let worker_ids = Arc::new(Mutex::new(Vec::new()));
        let pauses = [
            Arc::new(intercept::Pause::default()),
            Arc::new(intercept::Pause::default()),
        ];
        let mut tasks = Vec::new();
        for ((seed, display), pause) in [host_seed, viewer_seed]
            .into_iter()
            .zip(desktops.iter().map(|d| d.display.clone()))
            .zip(pauses.iter().cloned())
        {
            let ids = worker_ids.clone();
            let mut sequence = 100;
            tasks.push(
                seed.spawn(
                    move || {
                        ids.lock().unwrap().push(std::thread::current().id());
                        X11Clipboard::open(&display, &ProtocolLimits::ABSOLUTE, true)
                            .map(|clipboard| intercept::Native { clipboard, pause })
                    },
                    move || {
                        sequence += 1;
                        Ok(sequence)
                    },
                )
                .unwrap(),
            );
        }
        Self {
            cx,
            link,
            bridges: [host_bridge, viewer_bridge],
            tasks: tasks.try_into().ok().unwrap(),
            apps,
            host,
            viewer,
            grant: g,
            serial: 1,
            worker_ids,
            pauses,
            _input: input,
            desktops,
        }
    }
    async fn turn(&mut self) {
        self.serial += 1;
        authority::present(&self.cx, &mut self.viewer, self.grant, self.serial);
        for app in &mut self.apps {
            app.pump().unwrap();
        }
        let [h, c] = &mut self.bridges;
        let (a, b) = Box::pin(both(
            h.drive(&mut self.link.h, Duration::from_micros(200), || true),
            c.drive(&mut self.link.c, Duration::from_micros(200), || true),
        ))
        .await;
        assert_eq!(a.unwrap(), State::Active, "host {:?}", h.reason());
        assert_eq!(b.unwrap(), State::Active, "viewer {:?}", c.reason());
    }
    async fn transfer(&mut self, from: usize, text: &str, id: u64) {
        copy(&mut self.apps[from], text, id);
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            assert!(
                Instant::now() < until,
                "clipboard publication never completed"
            );
            self.turn().await;
            if let Some(result) = self.bridges[1 - from].take_received().unwrap() {
                assert!(
                    matches!(result,Received::Consumed(Some(r)) if r.publication == Publication::SubmittedToOs),
                    "unexpected {result:?}"
                );
                break;
            }
        }
        let mut reader = open(&self.desktops[1 - from].display);
        reader.begin_read().unwrap();
        loop {
            assert!(Instant::now() < until);
            self.turn().await;
            if let Some(read) = reader.poll_read().unwrap() {
                assert_eq!(read.as_str(), text);
                break;
            }
        }
        for _ in 0..20 {
            self.turn().await;
        }
        assert!(
            self.bridges[from].take_received().unwrap().is_none(),
            "must not echo into the source"
        );
    }
    async fn shutdown(&mut self) {
        self.bridges[0].retire(&mut self.link.h).unwrap();
        self.bridges[1].retire(&mut self.link.c).unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        while self.tasks.iter().any(|t| !t.is_finished()) {
            assert!(Instant::now() < until, "native cleanup did not finish");
            self.link.drive(&self.cx).await;
        }
        for task in &mut self.tasks {
            let _ = task.finish().unwrap();
        }
        assert!(!self.link.h.is_closed() && !self.link.c.is_closed());
        self.host
            .monitor()
            .deadline(fr_core::time::HostInstant::from_micros(clock(&self.cx)))
            .unwrap();
        // Ordinary control remains usable in BOTH directions after native cleanup.
        for host in [true, false] {
            let mut bytes = [0u8; 24];
            bytes[..4].copy_from_slice(b"FRD0");
            bytes[6..8].copy_from_slice(&0x0012u16.to_be_bytes());
            bytes[16..20].copy_from_slice(&7u32.to_be_bytes());
            // Records retired during native cleanup may still await the peer's
            // acknowledgement; Backpressure means retry the same record.
            loop {
                let (q, outbound) = if host {
                    (&mut self.link.h, self.link.hr.outbound)
                } else {
                    (&mut self.link.c, self.link.cr.outbound)
                };
                match q.send(
                    &self.cx,
                    Route::Stream(outbound),
                    &bytes,
                    clock(&self.cx) + 500_000,
                    || true,
                ) {
                    Ok(()) => break,
                    Err(fr_transport::quic::Error::Backpressure) => {
                        assert!(Instant::now() < until, "control stayed backpressured");
                        self.link.drive(&self.cx).await;
                    }
                    Err(error) => panic!("control send after cleanup: {error:?}"),
                }
            }
            let mut got = false;
            while !got {
                assert!(Instant::now() < until);
                self.link.drive(&self.cx).await;
                let (q, inbound) = if host {
                    (&mut self.link.c, self.link.cr.inbound)
                } else {
                    (&mut self.link.h, self.link.hr.inbound)
                };
                q.receive_ready(
                    &self.cx,
                    || true,
                    |r| r == Route::Stream(inbound),
                    |_, b| {
                        assert_eq!(b, bytes);
                        got = true;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            }
        }
    }
}
#[test]
fn x11_workers_exchange_empty_unicode_and_one_mib_over_actual_quic() {
    runtime().block_on(async {
        for text in [
            String::new(),
            "private 🦀 café\n\0tail".into(),
            "🦀".repeat(262_144),
        ] {
            for from in [0, 1] {
                let mut world = World::new(2048).await;
                world.transfer(from, &text, 1).await;
                {
                    let ids = world.worker_ids.lock().unwrap();
                    assert_eq!(ids.len(), 2);
                    assert_ne!(ids[0], ids[1]);
                    assert!(!ids.contains(&std::thread::current().id()));
                }
                world.shutdown().await;
            }
        }
    });
}

#[test]
fn successive_copies_in_both_directions_keep_the_original_workers_and_channel() {
    runtime().block_on(async {
        let mut w = World::new(2048).await;
        w.transfer(0, "first", 1).await;
        w.transfer(1, "second 🦀", 2).await;
        w.transfer(0, "second 🦀", 3).await;
        w.transfer(1, "", 4).await;
        w.shutdown().await;
    });
}

#[test]
fn blocked_native_preparation_is_fenced_without_blocking_control_or_overwriting_local_copy() {
    use std::sync::atomic::Ordering;
    runtime().block_on(async {
        let mut w = World::new(2048).await;
        w.transfer(0, "keep this local value", 1).await;
        w.pauses[0].enabled.store(true, Ordering::Release);
        copy(&mut w.apps[1], "remote value that must never publish", 2);
        let until = Instant::now() + Duration::from_secs(1);
        while !w.pauses[0].entered.load(Ordering::Acquire) {
            assert!(Instant::now() < until);
            w.turn().await;
        }
        assert!(!w.tasks[0].is_finished());
        w.bridges[0].retire(&mut w.link.h).unwrap();
        // Prove network progress while native preparation is STILL blocked.
        let mut bytes = [0u8; 24];
        bytes[..4].copy_from_slice(b"FRD0");
        bytes[6..8].copy_from_slice(&0x0012u16.to_be_bytes());
        bytes[16..20].copy_from_slice(&7u32.to_be_bytes());
        w.link
            .h
            .send(
                &w.cx,
                Route::Stream(w.link.hr.outbound),
                &bytes,
                clock(&w.cx) + 500_000,
                || true,
            )
            .unwrap();
        let mut got = false;
        while !got {
            assert!(Instant::now() < until);
            w.link.drive(&w.cx).await;
            w.link
                .c
                .receive_ready(
                    &w.cx,
                    || true,
                    |r| r == Route::Stream(w.link.cr.inbound),
                    |_, b| {
                        assert_eq!(b, bytes);
                        got = true;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        assert!(!w.tasks[0].is_finished());
        w.pauses[0].release.store(true, Ordering::Release);
        w.shutdown().await;
        let mut reader = open(&w.desktops[0].display);
        reader.begin_read().unwrap();
        loop {
            assert!(Instant::now() < until);
            w.apps[0].pump().unwrap();
            if let Some(text) = reader.poll_read().unwrap() {
                assert_eq!(text.as_str(), "keep this local value");
                break;
            }
            std::thread::sleep(Duration::from_micros(100));
        }
        assert!(w.bridges[0].take_received().unwrap().is_none());
    });
}

#[test]
fn dropping_an_unpolled_network_drive_fences_the_original_native_workers() {
    runtime().block_on(async {
        let mut w = World::new(2048).await;
        let pending = w.bridges[0].drive(&mut w.link.h, Duration::from_millis(1), || true);
        drop(pending);
        assert!(w.link.h.is_closed());
        assert!(!w.link.c.is_closed());
        for task in &w.tasks {
            task.stop();
        }
        let until = Instant::now() + Duration::from_secs(2);
        while w.tasks.iter().any(|task| !task.is_finished()) {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        for task in &mut w.tasks {
            let _ = task.finish().unwrap();
        }
    });
}

#[test]
fn switch_off_on_retires_only_clipboard_and_never_replays_queued_text() {
    runtime().block_on(async {
        for index in [0, 1] {
            for local in [true, false] {
                let mut w = World::new(2048).await;
                w.transfer(0, "published before disabling", 1).await;
                let (a, b) = w.bridges[index].switches();
                let switch = if local { a } else { b };
                switch.set_enabled(false);
                switch.set_enabled(true);
                let q = if index == 0 {
                    &mut w.link.h
                } else {
                    &mut w.link.c
                };
                assert_eq!(
                    w.bridges[index].service(q, || true).unwrap(),
                    State::Retired
                );
                assert!(w.bridges[index].is_retired());
                copy(&mut w.apps[index], "must not replay after reenable", 2);
                w.shutdown().await;
                assert!(w.bridges[1 - index].take_received().unwrap().is_none());
            }
        }
    });
}

#[test]
fn retirement_before_worker_start_never_opens_native_clipboard() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx).await;
        let _input = link.attach(&cx, 9, MediaRole::Input).await;
        let (h, _peer) = link.attach(&cx, 10, MediaRole::Clipboard).await;
        let g = authority::grant(&cx);
        let host = authority::host(g);
        let lane = ClipboardChannel::new(
            &link.h,
            h,
            Binding {
                session: g.request.parent.remote_session,
                lease: g.lease,
            },
        )
        .unwrap();
        let (mut bridge, seed) = Bridge::host(cx, &link.h, lane, &host, true).unwrap();
        bridge.retire(&mut link.h).unwrap();
        let result = seed.open::<X11Clipboard>(|| panic!("retired seed must not call the OS"));
        assert!(result.is_err());
        assert!(!link.h.is_closed());
    });
}

#[test]
fn foreign_connection_refusal_never_fences_either_worker_or_connection() {
    runtime().block_on(async {
        let mut w = World::new(2048).await;
        assert_eq!(
            w.bridges[0].retire(&mut w.link.c),
            Err(frd::clipboard_quic::Error::WrongConnection)
        );
        assert_eq!(
            w.bridges[0]
                .drive(&mut w.link.c, Duration::from_micros(200), || true)
                .await,
            Err(frd::clipboard_quic::Error::WrongConnection)
        );
        assert!(!w.bridges[0].is_retired());
        assert!(!w.link.h.is_closed() && !w.link.c.is_closed());
        w.transfer(0, "original workers still live", 1).await;
        w.shutdown().await;
    });
}
