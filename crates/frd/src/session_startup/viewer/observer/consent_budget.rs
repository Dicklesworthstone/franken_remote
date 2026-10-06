//! Consent uses the ORIGINAL finite bootstrap budget, not an active input
//! lease or a timeout reset by a notification, byte, native result or UI turn.
use super::*;
use asupersync::{runtime::RuntimeBuilder, types::Budget as RuntimeBudget};

#[test]
fn consent_budget_allows_human_startup_without_extending_native_stage_timeouts() {
    let runtime = RuntimeBuilder::current_thread().enable_platform_reactor(true).build().unwrap();
    let cx = runtime.request_cx_with_budget(RuntimeBudget::INFINITE);
    let policy = Policy::default();
    assert_eq!(policy.timeout, crate::host_run::approval::STARTUP_BUDGET);
    assert_eq!(policy.timeout, Duration::from_secs(30));
    assert_eq!(policy.network_turn, Duration::from_millis(5));
    let before = now(&cx).unwrap();
    let budget = Budget::new(cx.clone(), policy).unwrap();
    let after = now(&cx).unwrap();
    assert!((before + 30_000_000..=after + 30_000_000).contains(&budget.until));
    assert_eq!(budget.stage().unwrap(), Duration::from_secs(2));
    assert_eq!(budget.wait().unwrap(), Duration::from_millis(5));
    let original = budget.until;
    for _ in 0..32 {
        assert!(budget.live());
        assert!(budget.remaining().unwrap() <= policy.timeout);
        assert!(budget.stage().unwrap() <= Duration::from_secs(2));
        assert_eq!(budget.until, original, "progress cannot refresh consent");
    }
}

#[test]
fn consent_budget_preserves_explicit_shorter_caller_policy() {
    let runtime = RuntimeBuilder::current_thread().enable_platform_reactor(true).build().unwrap();
    let cx = runtime.request_cx_with_budget(RuntimeBudget::INFINITE);
    let before = now(&cx).unwrap();
    let policy = Policy { timeout: Duration::from_secs(1), ..Policy::default() };
    let budget = Budget::new(cx.clone(), policy).unwrap();
    let after = now(&cx).unwrap();
    assert!((before + 1_000_000..=after + 1_000_000).contains(&budget.until));
    assert!(budget.stage().unwrap() <= policy.timeout);
}

#[test]
fn consent_budget_expiry_cancels_the_original_attempt_and_cannot_be_revived() {
    let runtime = RuntimeBuilder::current_thread().enable_platform_reactor(true).build().unwrap();
    let cx = runtime.request_cx_with_budget(RuntimeBudget::INFINITE);
    let mut budget = Budget::new(cx.clone(), Policy::default()).unwrap();
    let now = now(&cx).unwrap();
    budget.until = now;
    assert_eq!(budget.remaining(), Err(Error::Expired));
    assert!(cx.is_cancel_requested());
    // Even a buggy late caller that assigns more time cannot restore this
    // original cancelled context. No replacement session is allocated here.
    budget.until = now + 30_000_000;
    assert!(budget.remaining().is_err());
    assert!(budget.wait().is_err());
    assert!(budget.stage().is_err());
    assert!(!budget.live());
}
