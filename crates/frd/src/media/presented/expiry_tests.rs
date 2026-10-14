//! The production age/deadline calculation with explicit source-clock fixtures.
use super::*;

fn sample(age_upper_us: u64) -> wire::Sample {
    wire::Sample {
        stamp: wire::Stamp {
            frame: 1,
            captured_us: 100,
            observed_us: 200,
            source: SourceObservation::QualifiedUnchanged,
        },
        age_upper_us,
    }
}

#[test]
fn delayed_service_charges_elapsed_age_without_moving_the_send_by() {
    let sampled_at = 10_000;
    let serviced_at = 40_000;
    let original = sample(20_000);
    let ViewSample::Visible(aged, at) = sample_at(
        ViewSample::Visible(original, sampled_at),
        serviced_at,
        wire::BASE_SOURCE_AGE_US,
    )
    .unwrap()
    else {
        panic!("still-useful sample was lost");
    };
    assert_eq!(at, serviced_at);
    assert_eq!(aged.stamp, original.stamp);
    assert_eq!(aged.age_upper_us, 50_000);
    assert_eq!(
        at + wire::BASE_SOURCE_AGE_US - aged.age_upper_us,
        sampled_at + wire::BASE_SOURCE_AGE_US - original.age_upper_us
    );
}

#[test]
fn a_sample_which_expires_between_sampling_and_service_is_unavailable() {
    for now in [230_000, 230_001, 2_000_000] {
        assert!(matches!(
            sample_at(
                ViewSample::Visible(sample(20_000), 0),
                now,
                wire::BASE_SOURCE_AGE_US,
            ),
            Ok(ViewSample::Unavailable)
        ));
    }
    assert!(matches!(
        sample_at(ViewSample::Pending, 7, 10),
        Ok(ViewSample::Pending)
    ));
    assert!(matches!(
        sample_at(ViewSample::Unavailable, 7, 10),
        Ok(ViewSample::Unavailable)
    ));
}

#[test]
fn sample_clock_regression_and_age_overflow_remain_errors() {
    assert!(matches!(
        sample_at(
            ViewSample::Visible(sample(20_000), 2),
            1,
            wire::BASE_SOURCE_AGE_US,
        ),
        Err(Error::Proof(fr_media::presented::Error::Clock))
    ));
    assert!(matches!(
        sample_at(
            ViewSample::Visible(sample(20_000), 0),
            u64::MAX,
            wire::BASE_SOURCE_AGE_US,
        ),
        Err(Error::Proof(fr_media::presented::Error::Overflow))
    ));
}
