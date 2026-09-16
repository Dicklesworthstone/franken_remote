use super::*;

#[test]
fn observation_never_outlives_initial_authority_or_three_second_budget() {
    for lifetime in [100, 10_000_000] {
        let input = owner(lifetime);
        let mut channel = session(&input, Role::Host);
        {
            let mut read = channel.observe(at(0)).unwrap();
            let end = lifetime.min(3_000_000);
            assert_eq!(read.deadline(), at(end));
            read.check(at(end - 1)).unwrap();
            assert!(read.check(at(end)).is_err());
            assert_eq!(
                read.finish(1, "stale", None, at(end)),
                Err(Error::Closed.into())
            );
        }
        assert_eq!(channel.retained_bytes(), 0);
    }
}
#[test]
fn both_switch_off_on_cycles_consume_observation_without_rebinding() {
    for local in [true, false] {
        let input = owner(10_000_000);
        let mut channel = session(&input, Role::Host);
        let switch = if local {
            channel.local_switch()
        } else {
            channel.peer_switch()
        };
        {
            let mut read = channel.observe(at(0)).unwrap();
            switch.set_enabled(false);
            switch.set_enabled(true);
            assert_eq!(read.check(at(1)), Err(Error::Disabled.into()));
            assert_eq!(
                read.finish(1, "stale", None, at(2)),
                Err(Error::Closed.into())
            );
        }
        assert!(!channel.is_closed());
        assert_eq!(channel.retained_bytes(), 0);
        // A truly new operation may start; the previous operation never resumes.
        channel
            .observe(at(3))
            .unwrap()
            .finish(2, "fresh", None, at(4))
            .unwrap();
        assert_eq!(channel.retained_bytes(), 5);
    }
}
#[test]
fn original_owner_revocation_and_clock_regression_fence_pending_observation() {
    for regress in [true, false] {
        let input = owner(10_000_000);
        let mut channel = session(&input, Role::Host);
        {
            let mut read = channel.observe(at(20)).unwrap();
            if !regress {
                input.monitor().revoke();
            }
            assert!(
                read.finish(1, "stale", None, at(if regress { 19 } else { 21 }))
                    .is_err()
            );
        }
        assert!(channel.is_closed());
        assert_eq!(channel.retained_bytes(), 0);
    }
}
#[test]
fn native_change_cancels_old_started_send_even_when_new_read_fails() {
    let input = owner(10_000_000);
    let mut channel = session(&input, Role::Host);
    let old = stamp(channel.offer(1, "old", None, at(0)).unwrap());
    let mut gate = Gate::default();
    assert_eq!(pump(&mut channel, &mut gate, 0), Pump::RecordAccepted);
    {
        let mut read = channel.observe(at(1)).unwrap();
        read.native_change(None, at(1)).unwrap();
        // Unsupported/no-selection/read failure means no new text. Drop must not
        // resurrect the older outgoing value now that the OS selection changed.
    }
    assert_eq!(channel.retained_bytes(), 0);
    assert_eq!(pump(&mut channel, &mut gate, 2), Pump::CancelAccepted(old));
    assert_eq!(pump(&mut channel, &mut gate, 2), Pump::Idle);
}
#[test]
fn observation_finish_is_single_use_even_when_offer_is_invalid() {
    let input = owner(10_000_000);
    let mut channel = session(&input, Role::Host);
    {
        let mut read = channel.observe(at(0)).unwrap();
        assert!(read.finish(0, "not queued", None, at(0)).is_err());
        assert_eq!(
            read.finish(1, "not retried", None, at(0)),
            Err(Error::Closed.into())
        );
    }
    assert_eq!(channel.retained_bytes(), 0);
}
#[test]
fn native_change_fences_incoming_before_text_read_but_exact_echo_does_not() {
    for genuine in [true, false] {
        let input = owner(10_000_000);
        let peer = owner(10_000_000);
        let mut from = session(&input, Role::Controller);
        let mut to = session(&peer, Role::Host);
        let mut platform = Platform::default();
        let published = stamp(from.offer(1, "published", None, at(0)).unwrap());
        deliver(&mut from, &mut to, &mut platform);
        let pending = stamp(from.offer(2, "incoming", None, at(0)).unwrap());
        let mut gate = Gate::default();
        for _ in 0..2 {
            assert_eq!(pump(&mut from, &mut gate, 0), Pump::RecordAccepted);
            assert!(
                to.receive(&gate.last, &mut platform, || at(0))
                    .unwrap()
                    .is_none()
            );
        }
        {
            let mut read = to.observe(at(0)).unwrap();
            read.native_change(if genuine { None } else { Some(published) }, at(0))
                .unwrap();
            // The text read may be slow/unsupported; its early revision signal
            // already determines whether the pending incoming copy is stale.
        }
        assert_eq!(pump(&mut from, &mut gate, 0), Pump::ItemAccepted(pending));
        let result = to.receive(&gate.last, &mut platform, || at(0));
        if genuine {
            assert_eq!(result, Err(Error::LocalChanged.into()));
            assert_eq!(platform.text, ["published"]);
        } else {
            assert_eq!(
                result.unwrap().unwrap().publication,
                Publication::SubmittedToOs
            );
            assert_eq!(platform.text, ["published", "incoming"]);
        }
    }
}
