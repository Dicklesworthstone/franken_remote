//! The real seed's read-only native guard, with the existing virtual timer and
//! original wire/core authority. No native publication or cleanup is inferred.
use super::*;

fn no_native_operation(fixture: &Fixture) {
    let os = fixture.os.lock().unwrap();
    assert_eq!(
        (os.opens, os.watches, os.changes, os.reads, os.publications),
        (1, 0, 0, 0, 0)
    );
}

#[test]
fn original_item_deadline_remains_exclusive_without_refreshing_or_closing_the_lane() {
    let fixture = Fixture::new(false);
    let until = HostInstant::from_micros(1_000);
    assert!(fixture.native_access.check(Some(until)));
    fixture.clock.advance_to(Time::from_nanos(999_000));
    assert!(fixture.native_access.check(Some(until)));
    fixture.clock.advance_to(Time::from_millis(1));
    assert!(!fixture.native_access.check(Some(until)));
    assert!(!fixture.native_access.check(Some(until)));
    // An expired item cannot reopen; independent native metadata still belongs
    // to the same live owner and does not acquire a replacement item deadline.
    assert!(fixture.native_access.check(None));
    assert!(fixture.shared.gate.is_open());
    no_native_operation(&fixture);
}

#[test]
fn either_privacy_switch_cycle_permanently_fences_the_original_native_guard() {
    for peer in [false, true] {
        let fixture = Fixture::new(false);
        assert!(fixture.native_access.check(None));
        let (local, remote) = fixture.shared.gate.switches();
        let switch = if peer { remote } else { local };
        let original = switch.state();
        switch.set_enabled(false);
        assert!(!fixture.native_access.check(None));
        switch.set_enabled(true);
        assert!(switch.is_enabled());
        assert_ne!(switch.state(), original);
        assert!(!fixture.native_access.check(None));
        assert!(
            !fixture
                .native_access
                .check(Some(HostInstant::from_micros(5_000_000)))
        );
        no_native_operation(&fixture);
    }
}

#[test]
fn clipboard_stop_cannot_be_revived_by_a_replacement_with_identical_numeric_scope() {
    let original = Fixture::new(false);
    assert!(original.native_access.check(None));
    original.shared.stop();
    assert!(!original.native_access.check(None));
    assert!(!original.permission.revoked.load(Ordering::Acquire));
    let replacement = Fixture::new(false);
    assert_eq!(
        original.shared.gate.context(),
        replacement.shared.gate.context()
    );
    assert!(replacement.native_access.check(None));
    assert!(!original.native_access.check(None));
    no_native_operation(&original);
    no_native_operation(&replacement);
}

#[test]
fn original_authority_revocation_refuses_metadata_and_publication_without_native_calls() {
    let fixture = Fixture::new(false);
    assert!(fixture.native_access.check(None));
    fixture.permission.revoked.store(true, Ordering::Release);
    assert!(!fixture.native_access.check(None));
    assert!(
        !fixture
            .native_access
            .check(Some(HostInstant::from_micros(5_000_000)))
    );
    no_native_operation(&fixture);
}

#[test]
fn view_resume_does_not_extend_a_retained_publication_deadline_or_expired_lease() {
    let fixture = Fixture::new(false);
    let until = HostInstant::from_micros(1_000);
    assert!(fixture.native_access.check(Some(until)));
    fixture.permission.paused.store(true, Ordering::Release);
    assert!(!fixture.native_access.check(None));
    fixture.clock.advance_to(Time::from_millis(1));
    fixture.permission.paused.store(false, Ordering::Release);
    assert!(fixture.native_access.check(None));
    assert!(!fixture.native_access.check(Some(until)));
    fixture.clock.advance_to(Time::from_millis(10_000));
    assert!(!fixture.native_access.check(None));
    assert!(
        !fixture
            .native_access
            .check(Some(HostInstant::from_micros(20_000_000)))
    );
    no_native_operation(&fixture);
}

#[test]
fn original_runtime_cancellation_fences_a_guard_that_outlives_the_check_caller() {
    let fixture = Fixture::new(false);
    assert!(fixture.native_access.check(None));
    fixture
        .worker
        .cx
        .cancel_with(asupersync::types::CancelKind::User, None);
    assert!(!fixture.native_access.check(None));
    no_native_operation(&fixture);
}
