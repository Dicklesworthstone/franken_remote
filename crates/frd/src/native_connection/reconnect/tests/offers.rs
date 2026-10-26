//! Optional removal policy and the real failed-discovery supervisor path.
use super::*;
use fr_core::limits::{LimitOverrides, ProtocolLimits};
use fr_wire::negotiation::Capability;

struct Choosing<F> {
    inner: App,
    choose: F,
    seen: Vec<(u8, Offer, usize, bool)>,
}
impl<F> Choosing<F> {
    fn new(choose: F) -> Self {
        Self {
            inner: App::new(),
            choose,
            seen: Vec::new(),
        }
    }
}
impl<F: FnMut(u8, &Offer) -> Result<Offer, CallbackError>> Application for Choosing<F> {
    type Output = ();
    fn offer(&mut self, attempt: u8, original: &Offer) -> Result<Offer, CallbackError> {
        let connecting = self.inner.events.last() == Some(&Status::Connecting { attempt });
        self.seen
            .push((attempt, original.clone(), self.inner.cleaned, connecting));
        (self.choose)(attempt, original)
    }
    async fn run(&mut self, attempt: u8, viewer: Viewer) -> Result<(), ObserverError> {
        self.inner.run(attempt, viewer).await
    }
    async fn cleanup(&mut self, cx: &Cx, deadline: Deadline) -> Result<(), CallbackError> {
        self.inner.cleanup(cx, deadline).await
    }
    fn status(&mut self, status: Status) -> Result<(), CallbackError> {
        self.inner.status(status)
    }
}
fn baseline() -> Offer {
    let offer = fr_client::native::control_offer_with_files();
    offer.validate().unwrap();
    offer
}
#[allow(clippy::unnecessary_wraps)] // This function is an Application::offer callback.
fn required_only(_: u8, original: &Offer) -> Result<Offer, CallbackError> {
    let mut result = original.clone();
    result.capabilities.retain(|cap| cap.required);
    Ok(result)
}
fn selected(app: &mut impl Application, original: &Offer) -> Result<Offer, Failure> {
    super::super::offer::prepare(app, 2, original)
}

#[test]
fn default_preserves_the_offer_and_optional_removal_keeps_every_required_entry() {
    let original = baseline();
    assert_eq!(selected(&mut App::new(), &original), Ok(original.clone()));
    let expected = required_only(2, &original).unwrap();
    assert!(expected.capabilities.len() < original.capabilities.len());
    assert_eq!(
        selected(&mut Choosing::new(required_only), &original),
        Ok(expected)
    );
    assert_eq!(original, baseline());
}

#[test]
fn capability_addition_change_downgrade_duplication_and_reordering_are_refused() {
    let original = baseline();
    let optional = original
        .capabilities
        .iter()
        .position(|c| !c.required)
        .unwrap();
    let required = original
        .capabilities
        .iter()
        .position(|c| c.required)
        .unwrap();
    let mut variants = Vec::new();

    let mut missing = original.clone();
    missing.capabilities.remove(required);
    variants.push(missing);
    for index in [required, optional] {
        let mut changed = original.clone();
        changed.capabilities[index].version += 1;
        variants.push(changed);
        let mut changed = original.clone();
        changed.capabilities[index].required = !changed.capabilities[index].required;
        variants.push(changed);
    }
    let mut added = original.clone();
    added.capabilities.push(Capability {
        name: "new-authority".into(),
        version: 1,
        required: false,
    });
    added.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    variants.push(added);
    let mut duplicate = original.clone();
    duplicate
        .capabilities
        .push(duplicate.capabilities[optional].clone());
    duplicate.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    variants.push(duplicate);
    let mut reversed = original.clone();
    reversed.capabilities.reverse();
    variants.push(reversed);
    let mut over_bound = original.clone();
    over_bound.capabilities.resize(
        fr_wire::negotiation::MAX_CAPABILITIES + 1,
        original.capabilities[0].clone(),
    );
    variants.push(over_bound);

    for changed in variants {
        let mut app = Choosing::new(move |_: u8, _: &Offer| Ok(changed.clone()));
        assert_eq!(selected(&mut app, &original), Err(Failure::Notification));
    }
}

#[test]
fn role_profile_versions_and_even_stricter_limits_cannot_change() {
    let original = baseline();
    let mut variants = Vec::new();
    let mut changed = original.clone();
    changed.role = Role::Observe;
    variants.push(changed);
    let mut changed = original.clone();
    changed.versions.push(1);
    variants.push(changed);
    let mut changed = original.clone();
    changed.profile += 1;
    variants.push(changed);
    let mut changed = original.clone();
    changed.profile_version += 1;
    variants.push(changed);
    let mut changed = original.clone();
    changed.limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(32_768),
        ..LimitOverrides::default()
    })
    .unwrap();
    changed.validate().unwrap();
    variants.push(changed);
    for changed in variants {
        let mut app = Choosing::new(move |_: u8, _: &Offer| Ok(changed.clone()));
        assert_eq!(selected(&mut app, &original), Err(Failure::Notification));
    }
}

#[test]
fn every_hook_sees_the_original_offer_after_connecting_and_previous_cleanup() {
    let rt = network::runtime();
    rt.block_on(async {
        let original = baseline();
        let cx = rt.request_cx_with_budget(Budget::INFINITE);
        let mut app = Choosing::new(required_only);
        let outcome = client()
            .run_control_capable(
                cx,
                rt.handle(),
                PeerSelector::StableId("absent"),
                Configuration::default(),
                original.clone(),
                quick(),
                &mut app,
            )
            .await;
        assert_eq!(
            outcome,
            Err(Failure::Connection(ConnectionError::Tailnet(
                fr_tailnet::Error::LocalApiUnavailable
            )))
        );
        assert_eq!(app.inner.cleaned, 3);
        assert_eq!(app.seen.len(), 3);
        for (index, (attempt, offered, cleaned, connecting)) in app.seen.iter().enumerate() {
            assert_eq!(usize::from(*attempt), index + 1);
            assert_eq!(*cleaned, index);
            assert!(*connecting);
            assert_eq!(offered, &original);
        }
    });
}

#[test]
fn refusal_or_invalid_change_stops_before_discovery_but_still_cleans_the_attempt() {
    let rt = network::runtime();
    rt.block_on(async {
        for refuse in [false, true] {
            for fail_cleanup in [false, true] {
                let cx = rt.request_cx_with_budget(Budget::INFINITE);
                let mut app = Choosing::new(move |_: u8, original: &Offer| {
                    if refuse {
                        return Err(CallbackError);
                    }
                    let mut changed = original.clone();
                    changed.capabilities.clear();
                    Ok(changed)
                });
                app.inner.fail_cleanup = fail_cleanup;
                let outcome = client()
                    .run_control_capable(
                        cx,
                        rt.handle(),
                        PeerSelector::StableId("absent"),
                        Configuration::default(),
                        baseline(),
                        quick(),
                        &mut app,
                    )
                    .await;
                let failure = if fail_cleanup {
                    Failure::Cleanup
                } else {
                    Failure::Notification
                };
                assert_eq!(outcome, Err(failure));
                assert_eq!(app.inner.cleaned, 1);
                assert_eq!(app.seen.len(), 1);
                assert_eq!(
                    app.inner.events,
                    [
                        Status::Connecting { attempt: 1 },
                        Status::Cleaning {
                            attempt: 1,
                            failure: Some(Failure::Notification)
                        },
                        Status::Stopped {
                            attempt: 1,
                            failure
                        },
                    ]
                );
            }
        }
    });
}

#[test]
fn cancellation_from_the_hook_is_observed_before_discovery_and_still_cleans() {
    let rt = network::runtime();
    rt.block_on(async {
        let cx = rt.request_cx_with_budget(Budget::INFINITE);
        let stop = cx.clone();
        let mut app = Choosing::new(move |_: u8, original: &Offer| {
            stop.cancel_fast(CancelKind::User);
            Ok(original.clone())
        });
        let outcome = client()
            .run_control_capable(
                cx,
                rt.handle(),
                PeerSelector::StableId("absent"),
                Configuration::default(),
                baseline(),
                quick(),
                &mut app,
            )
            .await;
        assert_eq!(outcome, Err(Failure::Cancelled));
        assert_eq!(app.inner.cleaned, 1);
        assert_eq!(app.seen.len(), 1);
        assert_eq!(
            app.inner.events,
            [
                Status::Connecting { attempt: 1 },
                Status::Cleaning {
                    attempt: 1,
                    failure: Some(Failure::Cancelled)
                },
                Status::Stopped {
                    attempt: 1,
                    failure: Failure::Cancelled
                },
            ]
        );
    });
}
