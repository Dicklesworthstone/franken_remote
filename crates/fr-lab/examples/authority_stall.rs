#![forbid(unsafe_code)]
//! Reproduce with: `cargo run -p fr-lab --example authority_stall -- 17`
//! Test-only synthetic action bytes; this is not a protocol/OS input demo.
use fr_core::authority::{AuthorityError, AuthorityPolicy, SessionAuthority};
use fr_core::ids::{InputLeaseId, InputTicketId, RemoteSessionId};
use fr_core::time::{HostDuration, HostInstant};
use fr_lab::{Destination, Fault, Limits, Scenario};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seed = std::env::args()
        .nth(1)
        .map_or(Ok(17), |s| s.parse::<u64>())?;
    eprintln!("fr-lab seed={seed}");
    let mut lab = Scenario::new(seed, Limits::default())?;
    let mut state = SessionAuthority::new(
        RemoteSessionId::from_raw(1),
        AuthorityPolicy::plan_defaults(),
    );
    let lease = InputLeaseId::from_raw(2);
    let ticket = InputTicketId::from_raw(3);
    state.mark_capabilities_checked().expect("new session");
    state
        .authorize_observation(HostInstant::ORIGIN)
        .expect("initial observation grant");
    state
        .mark_view_ready(HostInstant::ORIGIN)
        .expect("live view");
    state
        .grant_lease(lease, HostInstant::ORIGIN)
        .expect("available controller slot");
    state
        .issue_input_ticket(lease, ticket, HostInstant::ORIGIN)
        .expect("live lease");

    lab.send(
        Destination::Host,
        b"synthetic-action",
        Fault::after(HostDuration::from_micros(100_000)),
    )?;
    lab.elapse(HostDuration::from_micros(1_500_000))?;
    let mut result = None;
    lab.drain(|delivery| {
        result = Some(state.authorize_submission(lease, ticket, delivery.now));
        u16::from(result.as_ref().is_some_and(Result::is_err))
    })?;
    println!("{lab:?}");
    assert_eq!(
        result,
        Some(Err(AuthorityError::TicketExpired)),
        "seed={seed}: expired action admitted"
    );
    println!(
        "seed={seed}: actual fr-core submission check refused the expired ticket; no OS input was submitted"
    );
    Ok(())
}
