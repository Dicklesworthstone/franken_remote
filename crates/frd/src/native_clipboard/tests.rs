//! Application cause/cleanup ordering with an explicit already-retired endpoint.
//! A stopped seed runs a real worker thread; no native factory or OS effect runs.
use super::*;
use crate::test_support::{
    attachment, authority,
    transport_support::{clock, runtime},
};
use fr_core::clipboard::{Binding as Scope, Endpoint as Source, Publication, Receipt, Stamp};
use fr_transport::quic::clipboard::ClipboardChannel;
use fr_wire::attachment::MediaRole;

struct Retired {
    reason: Option<Error>,
    receipt: Option<Received>,
}
impl Endpoint for Retired {
    fn retire(&mut self) -> Result<(), Error> {
        Ok(())
    }
    fn retired(&self) -> bool {
        true
    }
    fn reason(&self) -> Option<Error> {
        self.reason
    }
    fn take_worker(&mut self) -> Option<WorkerSeed> {
        panic!("retired endpoint cannot open a worker")
    }
    fn switches(&self) -> Option<(ClipboardSwitch, ClipboardSwitch)> {
        None
    }
    fn received(&mut self) -> Result<Option<Received>, Error> {
        Ok(self.receipt.take())
    }
}

fn application(cleanup: Result<(), Error>) -> Application {
    let mut app = Application::new(Configuration {
        timeout: Duration::from_secs(2),
        consent: true,
        launch: None,
        before_native: None,
    });
    app.started = true;
    // Exact externally visible state after collect_cleanup collected the
    // original worker, before turn observes the endpoint's retired tombstone.
    {
        let mut state = app.control.lock();
        state.status.phase = Phase::Running;
        state.status.cleanup = Cleanup::Finished(cleanup);
        state.status.reason = cleanup.err();
    }
    app
}

fn collect(app: &mut Application, reason: Option<Error>) {
    let receipt = Received::Consumed(Some(Receipt {
        stamp: Stamp {
            id: 901,
            source: Source::Host,
            sequence: 1,
        },
        publication: Publication::SubmittedToOs,
    }));
    let mut endpoint = Retired {
        reason,
        receipt: Some(receipt),
    };
    app.turn(&mut endpoint, |_, _, _| {
        panic!("original attachment must not restart")
    })
    .unwrap();
    assert_eq!(app.control.status().phase, Phase::Retired);
    assert_eq!(app.control.take_received(), Some(receipt));
    assert!(app.control.take_received().is_none());
    app.close();
}

#[test]
fn receive_expiry_precedes_collection_of_a_finished_fenced_worker() {
    runtime().block_on(Box::pin(async {
        let cx = Cx::current().unwrap();
        let mut link = attachment::Link::new(&cx).await;
        let _input = link.attach(&cx, 9, MediaRole::Input).await;
        let (h, _c) = link.attach(&cx, 10, MediaRole::Clipboard).await;
        let grant = authority::grant(&cx);
        let host = authority::host(grant);
        let lane = ClipboardChannel::new(
            &link.h,
            h,
            Scope {
                session: grant.request.parent.remote_session,
                lease: grant.lease,
            },
        )
        .unwrap();
        let (mut bridge, seed) =
            crate::clipboard_quic::Bridge::host(cx.clone(), &link.h, lane, &host, true).unwrap();
        seed.control().stop();
        let task = seed
            .spawn::<crate::clipboard_process::RemoteClipboard, _, _>(
                || panic!("fenced seed must never open a native clipboard"),
                || Ok(901),
            )
            .unwrap();
        let mut app = application(Ok(()));
        app.task = Some(task);
        app.control.lock().status.cleanup = Cleanup::Pending;
        let until = clock(&cx) + 1_000_000;
        while !app.task.as_ref().unwrap().is_finished() {
            assert!(clock(&cx) < until);
            asupersync::time::sleep(cx.now(), Duration::from_millis(1)).await;
        }
        // The actual thread has completed, but turn has not collected it yet.
        collect(&mut app, Some(Error::ReceiveExpired));
        let status = app.control.status();
        assert_eq!(status.reason, Some(Error::ReceiveExpired));
        assert_eq!(
            status.cleanup,
            Cleanup::Finished(Err(Error::Session(
                fr_wire::clipboard::session::SessionError::Clipboard(
                    fr_core::clipboard::Error::Closed
                )
            )))
        );
        assert!(app.task.is_none());
        bridge.retire(&mut link.h).unwrap();
    }));
}

#[test]
fn receive_expiry_does_not_replace_a_specific_earlier_native_or_authority_failure() {
    for earlier in [
        Error::Native,
        Error::NativeSetup(PlatformError::Unavailable),
        Error::HandoffExpired,
        Error::Cancelled,
        Error::SwitchChanged,
        Error::Controller(fr_client::clipboard::Error::Expired),
    ] {
        let mut app = application(Err(earlier));
        collect(&mut app, Some(Error::ReceiveExpired));
        let status = app.control.status();
        assert_eq!(status.reason, Some(earlier));
        assert_eq!(status.cleanup, Cleanup::Finished(Err(earlier)));
    }
}

#[test]
fn an_ordinary_retirement_cannot_replace_a_collected_worker_failure() {
    for reason in [None, Some(Error::Closed), Some(Error::SwitchChanged)] {
        let mut app = application(Err(Error::Closed));
        collect(&mut app, reason);
        assert_eq!(app.control.status().reason, Some(Error::Closed));
    }
}
