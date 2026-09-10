use fr_core::ids::HostBootId;
use fr_media::freshness::{ClockCorrelation, ClockPolicy, ClockSample, Error};
#[test]
fn deadline_uses_whole_exchange_and_future_drift_without_origin_assumptions() {
    for host in [0, 1_000_000, u64::MAX - 10_000_000] {
        for drift in [0, 1, 1000, 10_000] {
            let sample = ClockSample {
                host_boot: HostBootId::from_raw(1),
                host_sample_us: host,
                client_sent_us: 100,
                client_received_us: 800,
            };
            let c = ClockCorrelation::new(
                sample,
                ClockPolicy {
                    drift_ppm: drift,
                    ..ClockPolicy::default()
                },
            )
            .unwrap();
            for now in [800, 1500, 80_000] {
                let host_end = host + 1_000_000;
                let until = c.deadline_lower_us(host_end, now).unwrap();
                assert!(until <= 1_000_100);
                assert!(until > now);
                assert!(
                    c.age_upper_us(0, until - 1).unwrap() < host_end,
                    "exclusive local deadline must precede host expiry"
                );
                if drift == 0 {
                    assert_eq!(until, 1_000_100);
                }
            }
        }
    }
}
#[test]
fn expired_clock_deadline_and_overflow_fail_closed() {
    let c = ClockCorrelation::new(
        ClockSample {
            host_boot: HostBootId::from_raw(1),
            host_sample_us: 1_000,
            client_sent_us: 0,
            client_received_us: 100,
        },
        ClockPolicy {
            valid_for_us: 1000,
            drift_ppm: 0,
            ..ClockPolicy::default()
        },
    )
    .unwrap();
    assert_eq!(c.deadline_lower_us(1100, 100), Ok(100));
    assert_eq!(c.deadline_lower_us(20_000, 100), Ok(1100));
    assert_eq!(c.deadline_lower_us(20_000, 99), Err(Error::ClockRegression));
    assert_eq!(c.deadline_lower_us(20_000, 1100), Err(Error::ClockExpired));
    let overflow = ClockCorrelation::new(
        ClockSample {
            host_boot: HostBootId::from_raw(1),
            host_sample_us: u64::MAX,
            client_sent_us: 0,
            client_received_us: 1,
        },
        ClockPolicy::default(),
    )
    .unwrap();
    assert_eq!(
        overflow.deadline_lower_us(u64::MAX, 1),
        Err(Error::ClockOverflow)
    );
}
