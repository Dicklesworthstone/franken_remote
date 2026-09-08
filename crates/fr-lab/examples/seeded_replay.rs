#![forbid(unsafe_code)]
//! Replay a toy receiver through seeded loss, duplication, delay and reordering.
//! Run `cargo run -p fr-lab --example seeded_replay -- 31` twice to compare logs.
use fr_core::time::HostDuration;
use fr_lab::{Destination, Fault, Limits, Scenario, TraceEvent};

fn run(seed: u64) -> Result<(u64, Vec<TraceEvent>), fr_lab::Failure> {
    let mut lab = Scenario::new(seed, Limits::default())?;
    for action in 0_u8..40 {
        lab.send(
            Destination::Client,
            &[action],
            Fault::Seeded {
                max_delay: HostDuration::from_micros(1_000),
                loss_per_million: 250_000,
                duplicate_per_million: 500_000,
            },
        )?;
    }
    let mut state = 0_u64;
    lab.advance(HostDuration::from_micros(1_000), |delivery| {
        // Order-sensitive toy state; these synthetic bytes are not wire records.
        for action in delivery.payload {
            state = state.rotate_left(5) ^ u64::from(*action);
        }
        0
    })?;
    Ok((state, lab.trace().to_vec()))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seed = std::env::args()
        .nth(1)
        .map_or(Ok(31), |value| value.parse::<u64>())?;
    eprintln!("fr-lab seed={seed}");
    let first = run(seed)?;
    let second = run(seed)?;
    assert_eq!(first, second, "seed={seed}: replay diverged");
    println!("seed={seed} toy_state={} schedule={:?}", first.0, first.1);
    Ok(())
}
