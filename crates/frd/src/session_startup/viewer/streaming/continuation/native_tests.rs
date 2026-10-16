//! Recovery's canonical native wait and original authenticated QUIC parent.
//! The native completion is a scheduled future, not codec or pixel evidence.
use super::*;
use crate::session_startup::{
    running::{
        controlled::tests::attach,
        tests::{pair_initialized, run},
    },
    viewer::streaming::StreamingViewerControl,
};
use fr_wire::negotiation::Capability;
use std::{
    cell::RefCell,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

#[derive(Default)]
struct Calls {
    polls: u32,
    dropped_after_fence: Option<bool>,
}
struct Native {
    cx: Cx,
    calls: Rc<RefCell<Calls>>,
    result: Result<u64, decoder_startup::Error>,
}
impl Future for Native {
    type Output = Result<u64, decoder_startup::Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let mut calls = self.calls.borrow_mut();
        calls.polls += 1;
        if calls.polls == 1 {
            task.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(self.result)
        }
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        self.calls.borrow_mut().dropped_after_fence = Some(self.cx.is_cancel_requested());
    }
}

#[allow(clippy::too_many_lines)]
fn exercise(native_failure: bool) {
    run(|c, h| async move {
        let mut caps: Vec<_> = [
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
        caps.sort_by(|a, b| a.name.cmp(&b.name));
        let (mut host, mut viewer) = pair_initialized(&c, &h, caps, |_| {}).await;
        let (_, config) =
            attach(&mut host, &mut viewer, &c, &h, MediaRole::Configuration, 18).await;
        let (_, recovery) = attach(&mut host, &mut viewer, &c, &h, MediaRole::Recovery, 19).await;
        let (_, video) = attach(&mut host, &mut viewer, &c, &h, MediaRole::Video, 20).await;
        let media = NegotiatedMedia::new(
            &viewer.transport,
            host.selection(),
            &config,
            &recovery,
            &video,
        )
        .unwrap();
        let original = viewer.transport.binding();
        let early = viewer.control();
        let mut old = media.binding();
        old.parent = viewer.opened.binding;
        let next = Binding {
            recovery: old.recovery.next().unwrap(),
            ..old
        };
        let refused = Cell::new(None);
        let [(recovery, _), (video, _), (datagram, _)] =
            media.viewer_routes(&viewer.transport).unwrap();
        let dispatch = Dispatch {
            inbound: Route::Stream(viewer.routes.inbound),
            old,
            next,
            metrics: false,
            limits: viewer.opened.selection.limits,
            obsolete: [
                Route::Stream(media.viewer_configuration_route(&viewer.transport).unwrap()),
                recovery,
                video,
                datagram,
            ],
            active: None,
            refused: &refused,
        };
        let budget = Budget {
            cx: c.clone(),
            until: now(&c).unwrap() + 1_000_000,
        };
        let calls = Rc::new(RefCell::new(Calls::default()));
        let native = Native {
            cx: c.clone(),
            calls: calls.clone(),
            result: if native_failure {
                Err(decoder_startup::Error::UnsupportedConfiguration)
            } else {
                Ok(17)
            },
        };
        let stop = StreamingViewerControl::observing(c.clone());
        let mut callbacks = 0;
        let result = during(
            &mut Link {
                parent: Parent::Observe(&mut viewer),
                audio: None,
                refused: &refused,
            },
            &budget,
            native,
            &dispatch,
            &mut |_, event| {
                assert!(event.is_none(), "native wait synthesized a frame event");
                assert!(!stop.is_stopped(), "UI ran after failure fence");
                callbacks += 1;
                if native_failure {
                    Ok(())
                } else {
                    Err(())
                }
            },
            &mut |_, _| Ok(Disposition::Blocked),
        )
        .await;
        let expected = if native_failure {
            Error::Startup(decoder_startup::Error::UnsupportedConfiguration)
        } else {
            Error::Application
        };
        assert_eq!(result, Err(expected));
        assert!(early.is_stopped());
        assert!(viewer.transport.is_bound_to(&original));
        assert_eq!(calls.borrow().dropped_after_fence, Some(true));
        if native_failure {
            assert_eq!(calls.borrow().polls, 2);
        } else {
            assert_eq!(callbacks, 1);
        }
        viewer.close();
        host.close();
    });
}

#[test]
fn recovery_native_failure_fences_the_original_authenticated_parent_before_native_drop() {
    exercise(true);
}

#[test]
fn recovery_ui_refusal_fences_the_parent_before_pending_or_completed_native_drop() {
    exercise(false);
}
