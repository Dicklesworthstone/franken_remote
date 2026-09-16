//! The real running-session drive loops, not manual Bridge service calls.
//! UDP/TLS, startup and clock exchanges are real. Grant/OS data are fixtures.
use super::*;
use fr_core::clipboard::{ClipboardSink, Publication, Stamp};
use fr_wire::clipboard::session::synchronize::{
    NativeChange, NativeChanges, NativeClipboard, NativeText, Received,
};

pub(super) fn accepted_grant(
    parent: negotiation::ControlBinding,
    capabilities: Capabilities,
    correlation: fr_media::freshness::ClockCorrelation,
    c: &Cx,
    ticket: input_ticket::Ticket,
) -> InputClient {
    let t = ClientInstant(now(c).unwrap());
    let at = ticket.issued_at_us;
    assert_eq!(ticket.sequence, 0);
    let g = control::Granted {
        request: control::Request {
            parent,
            sequence: 1,
            target: control::Target {
                display_binding: 8,
                view: creds().view,
                bounds: bounds(),
                capabilities,
            },
        },
        input_channel: 10,
        lease: creds().lease,
        ticket: ticket.credentials.ticket,
        issued_at_us: at,
        lease_until_us: at + 2_000_000,
        ticket_until_us: ticket.expires_at_us,
        first_action: 0,
        first_pointer: 0,
    };
    let mut r =
        fr_client::control_grant::RequestControl::new(g.request, 10, ProtocolLimits::ABSOLUTE, t)
            .unwrap();
    r.sent(t).unwrap();
    let mut bytes = [0; control::GRANTED_BYTES];
    control::encode_granted(
        g,
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        fr_wire::input::InputDirection::HostToViewer,
        fr_wire::input::InputDelivery::Reliable,
    )
    .unwrap();
    r.accept(
        &bytes,
        correlation,
        Policy::default(),
        ClientInstant(now(c).unwrap()),
    )
    .unwrap()
    .1
}
#[derive(Default)]
struct Os {
    revision: u64,
    text: Option<String>,
    origin: Option<Stamp>,
    published: Vec<String>,
    opened: bool,
    closed: bool,
}
fn copy(os: &Arc<Mutex<Os>>, text: &str) {
    let mut os = os.lock().unwrap();
    os.revision += 1;
    os.text = Some(text.into());
    os.origin = None;
}
struct Text(String, Option<Stamp>);
impl NativeText for Text {
    fn text(&self) -> &str {
        &self.0
    }
    fn origin(&self) -> Option<Stamp> {
        self.1
    }
}
struct Native {
    os: Arc<Mutex<Os>>,
    seen: u64,
    reading: bool,
}
impl ClipboardSink for Native {
    fn prepare(&mut self, _: &str, _: Stamp) -> Result<(), fr_core::clipboard::PlatformError> {
        Ok(())
    }
    fn publish(&mut self, text: &str, stamp: Stamp) -> Publication {
        let mut os = self.os.lock().unwrap();
        os.text = Some(text.into());
        os.origin = Some(stamp);
        os.revision += 1;
        os.published.push(text.into());
        self.reading = false;
        Publication::SubmittedToOs
    }
}
impl NativeClipboard for Native {
    type Text = Text;
    type Error = ();
    fn watch(&mut self) -> Result<u64, ()> {
        Ok(self.seen)
    }
    fn revision(&self) -> u64 {
        self.os.lock().unwrap().revision
    }
    fn changes(&mut self) -> Result<NativeChanges, ()> {
        let os = self.os.lock().unwrap();
        let latest = (os.revision != self.seen).then_some(NativeChange {
            revision: os.revision,
            has_selection: os.text.is_some(),
            origin: os.origin,
        });
        self.seen = os.revision;
        Ok(NativeChanges {
            latest,
            settled: true,
        })
    }
    fn prepare_for_revision(
        &mut self,
        _: &str,
        _: Stamp,
        revision: u64,
    ) -> Result<(), fr_core::clipboard::PlatformError> {
        if self.revision() == revision {
            Ok(())
        } else {
            Err(fr_core::clipboard::PlatformError::Unavailable)
        }
    }
    fn begin_read(&mut self) -> Result<(), ()> {
        self.reading = true;
        Ok(())
    }
    fn poll_read(&mut self) -> Result<Option<Text>, ()> {
        if !self.reading {
            return Ok(None);
        }
        self.reading = false;
        let os = self.os.lock().unwrap();
        Ok(os.text.as_ref().map(|s| Text(s.clone(), os.origin)))
    }
    fn cancel_read(&mut self) {
        self.reading = false;
    }
    fn suspend(&mut self) -> Result<(), ()> {
        self.reading = false;
        Ok(())
    }
    fn close(&mut self) {
        self.os.lock().unwrap().closed = true;
    }
}
async fn setup(c: &Cx, h: &Cx) -> Fixture {
    Box::pin(fixture_with_clipboard(
        c,
        h,
        caps(),
        false,
        false,
        false,
        true,
    ))
    .await
}
fn open(
    seed: crate::clipboard_quic::WorkerSeed,
    os: &Arc<Mutex<Os>>,
) -> crate::clipboard_quic::WorkerTask {
    let os = os.clone();
    let mut n = 0u128;
    seed.spawn(
        move || {
            os.lock().unwrap().opened = true;
            Ok(Native {
                os,
                seen: 0,
                reading: false,
            })
        },
        move || {
            n += 1;
            Ok(n)
        },
    )
    .unwrap()
}
async fn drive(state: &mut Fixture, client: &Cx, host: &Cx, nonce: &mut u128, ticket: &mut u128) {
    let (host_result, viewer_result) = turn(state, client, host, nonce, ticket).await;
    host_result.unwrap();
    viewer_result.unwrap();
}
#[test]
fn running_sessions_transfer_both_directions_then_retire_without_stopping_input() {
    run(|c, h| async move {
        let mut state = setup(&c, &h).await;
        let (hc, vc) = state.clipboard_channels.take().unwrap();
        let hs = state.host.attach_clipboard(hc, true).unwrap();
        let vs = state.viewer.attach_clipboard(vc, true).unwrap();
        let os = [
            Arc::new(Mutex::new(Os::default())),
            Arc::new(Mutex::new(Os::default())),
        ];
        let mut workers = [open(hs, &os[0]), open(vs, &os[1])];
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(async {
            let mut n = 80_000; let mut t = 90_000;
            for (from,text) in [(0,"host clipboard"),(1,"viewer unicode λ"),(0,"")] {
                copy(&os[from],text);
                let end = now(&c).unwrap()+1_000_000;
                loop {
                    assert!(now(&c).unwrap()<end,"clipboard did not cross regular session drives");
                    drive(&mut state,&c,&h,&mut n,&mut t).await;
                    let receipt = if from==0 { state.viewer.take_clipboard_received() } else { state.host.take_clipboard_received() }.unwrap();
                    if let Some(receipt) = receipt {
                        assert!(matches!(receipt,Received::Consumed(Some(r)) if r.publication==Publication::SubmittedToOs));
                        assert_eq!(os[1-from].lock().unwrap().text.as_deref(),Some(text));
                        break;
                    }
                }
                for _ in 0..5 { drive(&mut state,&c,&h,&mut n,&mut t).await; }
            }
            assert_eq!(os[0].lock().unwrap().published.len(),1,"no echo");
            assert_eq!(os[1].lock().unwrap().published.len(),2,"no echo");
            state.viewer.clipboard_switches().unwrap().0.set_enabled(false);
            for _ in 0..20 { drive(&mut state,&c,&h,&mut n,&mut t).await; }
            assert!(state.viewer.clipboard_retired() && state.host.clipboard_retired());
            assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
            let _ = state.viewer.action(key(true)).unwrap();
            let end=now(&c).unwrap()+500_000;
            while state.viewer.pending_actions()!=0 {
                assert!(now(&c).unwrap()<end);
                drive(&mut state,&c,&h,&mut n,&mut t).await;
            }
            assert_eq!(state.effects.lock().unwrap().keys,[true]);
            state.viewer.close(); state.host.close();
        }, driver)).await;
        assert!(done.handoff_safe());
        for worker in &mut workers {
            worker.stop();
        }
    });
}
#[test]
fn unpolled_session_drives_fence_clipboard_before_native_open() {
    for host in [true, false] {
        run(move |c, h| async move {
            let mut state = setup(&c, &h).await;
            let (hc, vc) = state.clipboard_channels.take().unwrap();
            let hs = state.host.attach_clipboard(hc, true).unwrap();
            let vs = state.viewer.attach_clipboard(vc, true).unwrap();
            if host {
                drop(state.host.drive(
                    Duration::ZERO,
                    || Ok(80_000),
                    || Some(InputTicketId::from_raw(90_000)),
                    block,
                ));
            } else {
                drop(state.viewer.drive(Duration::ZERO, |_| {}, block));
            }
            let seed = if host {
                drop(vs);
                hs
            } else {
                drop(hs);
                vs
            };
            let opened = std::cell::Cell::new(false);
            let result = seed.open(|| {
                opened.set(true);
                Ok(Native {
                    os: Arc::default(),
                    seen: 0,
                    reading: false,
                })
            });
            assert!(result.is_err());
            assert!(!opened.get());
            state.viewer.close();
            state.host.close();
            assert!(state.driver.take().unwrap().await.handoff_safe());
        });
    }
}
#[test]
fn clipboard_consent_is_independent_of_the_accepted_control_grant() {
    run(|c, h| async move {
        let mut state = setup(&c, &h).await;
        let (hc, vc) = state.clipboard_channels.take().unwrap();
        assert!(state.host.attach_clipboard(hc, false).is_err());
        assert!(state.viewer.attach_clipboard(vc, false).is_err());
        assert!(state.host.clipboard_switches().is_none());
        assert!(state.viewer.clipboard_switches().is_none());
        assert!(!state.viewer.is_closed());
        assert!(!state.host.control().is_stopped());
        assert!(!state.host.io().unwrap().0.is_closed());
        state.viewer.close();
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
