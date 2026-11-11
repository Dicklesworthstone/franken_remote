# Native child readiness and stop

The host's session monitor and observation/microphone indicator supervisors wait
on their existing private command descriptors instead of waking every 5 ms. The
indicator also waits on its original terminal-signal datagram socket. A private
socket pair carries a permanent local shutdown so an explicit stop wakes an
outstanding wait; there is no growing wake-message queue or lost wake between
a status check and a poll.

Only the existing native-I/O worker threads perform these waits. The runtime
and authority paths continue to check bounded shared state. A readable descriptor
never means renewed authority: every return, including an interrupted poll,
rechecks the original owner before further IPC. Command bytes and local-revoke
datagrams remain with their existing protocol parsers.

The 50 ms query cadence, 250 ms request deadlines, and original source-evidence
bounds are unchanged. Each wait is capped by the earlier of its original
transaction/cadence deadline and current evidence expiry. Partial replies and
interruptions cannot start a replacement deadline. EOF, malformed or unsolicited
replies, local revoke, and explicit stop remain terminal; the original child
and worker permit stay owned through collection.

This implements part of `fr-idle-frd-cpu-icui`. It does not change the ingress
verification interval, native platform query cadence, or transport polling.
No process-family idle-CPU target is claimed from this source change.

## Verification

Regression sources cover actual private socket readiness, permanent stop wakes,
command hangup, unchanged timeout ownership, and preservation of unread reply
and terminal-signal bytes. The existing real subprocess suites retain their
failure and cleanup checks and add partial-reply stalls.

The following checks require an available build environment:

```bash
cargo test -p frd native_wait::tests
cargo test -p frd host_indicator::process
cargo test -p frd session_monitor
cargo test -p frd --test session_monitor
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

These new checks have not run in the creating session: the execution environment
became unavailable during implementation. Native desktop and process-family
CPU measurements remain separate qualification work.
