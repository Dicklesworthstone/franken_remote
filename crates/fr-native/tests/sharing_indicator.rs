//! Real XCB mapping on a test X server; observation authority is a fixture.
#![cfg(all(target_os = "linux", feature = "linux-session-ui"))]
#![forbid(unsafe_code)]
use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::RemoteSessionId,
};
use fr_native::sharing_indicator::{SharingIndicator, Status, StopReason};
use frd::media::{ObservationControl, host_now};
use std::{
    thread,
    time::{Duration, Instant},
};
fn display() -> Option<String> {
    if let Ok(display) = std::env::var("DISPLAY") {
        Some(display)
    } else {
        assert!(
            std::env::var_os("FR_NATIVE_INDICATOR_REQUIRED").is_none(),
            "required indicator tests need an X11 display"
        );
        eprintln!("BLOCKED: no X11 display; no native mapping qualification");
        None
    }
}
fn lifetime() -> (Runtime, Cx, ObservationControl) {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let source = rt.request_cx_with_budget(Budget::INFINITE);
    let mut authority = SessionAuthority::new(
        RemoteSessionId::from_raw(1),
        AuthorityPolicy::plan_defaults(),
    );
    authority.mark_capabilities_checked().unwrap();
    authority
        .authorize_observation(host_now(&source).unwrap())
        .unwrap();
    let observation = ObservationControl::new(source, authority).unwrap();
    (rt, cx, observation)
}
fn await_finished(indicator: &mut SharingIndicator) {
    let until = Instant::now() + Duration::from_secs(2);
    while indicator.finish().is_none() {
        assert!(
            Instant::now() < until,
            "original indicator thread did not exit"
        );
        thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn async_mapping_wait_retains_the_real_native_indicator_after_readiness() {
    let Some(display) = display() else { return };
    let (rt, cx, observation) = lifetime();
    let mut indicator = SharingIndicator::start(&display, observation.clone()).unwrap();
    rt.block_on(indicator.wait_until_mapped(&cx)).unwrap();
    assert_eq!(indicator.control().status(), Status::Mapped);
    assert!(indicator.control().window().is_some());
    assert!(observation.check().is_ok());
    indicator.control().stop();
    await_finished(&mut indicator);
    assert!(observation.check().is_err());
    assert_eq!(indicator.finish(), Some(StopReason::User));
}
#[test]
fn unpolled_async_mapping_retains_real_worker_cleanup_ownership() {
    let Some(display) = display() else { return };
    let (_rt, cx, observation) = lifetime();
    let mut indicator = SharingIndicator::start(&display, observation.clone()).unwrap();
    drop(indicator.wait_until_mapped(&cx));
    assert!(observation.check().is_err());
    assert!(cx.checkpoint().is_ok());
    // Revocation is not proof of native thread exit. Collect the original owner.
    await_finished(&mut indicator);
    assert_eq!(
        indicator.control().status(),
        Status::Stopped(StopReason::OwnerDropped)
    );
}
