# Local control of a foreground connection

The Linux CLI implements `fr disconnect TARGET` and
`fr robot session close TARGET` against an existing foreground `fr connect`.
The commands stop that original invocation, including an in-progress dial or a
viewing reconnect, through its normal cancellation and native cleanup path.
They do not start another host session, signal a PID, reacquire control, or replay
input and file selections.

## Use

Start a connection normally, then use either command from another terminal as
the same operating-system user:

```bash
fr disconnect NODE_ID --json
fr robot session close NODE_ID --json
```

Use the exact target text passed to `fr connect`. For a connection started with
`--by-name`, use that exact canonical name. Prefixes and alternate aliases are
not inferred. One foreground invocation may own a given exact target per user;
a second invocation receives `local_session_already_running` while the original
is active or cleaning up. Different users and different targets are independent.

Local closure does not require Tailscale to be running, a working LocalAPI socket,
or host connectivity. Without a matching local owner, `no_local_session` is a
refusal and nothing is stopped. A supplied robot `--lease` is refused as
`local_lease_unavailable`: no robot lease handle has been issued, and an arbitrary
handle must never be silently ignored or accepted as authority.

Explicit foreground robot session-open and read-only observation publication
use this same local ownership boundary. An opt-in bounded native input mode is
documented in [NATIVE_ROBOT_INPUT.md](NATIVE_ROBOT_INPUT.md). Detached robot
broker workflows remain unavailable; no background client service is created.

## Ownership and bounds

The original invocation binds a Linux abstract Unix socket. The kernel removes
its binding when the owner closes; no socket path, PID file, discovery database,
or stale filesystem entry can point to a later process. The routing name includes
the kernel-derived user ID and a hash of the target. The hash is routing metadata,
not authentication: both endpoints check kernel peer credentials, and the client
checks the full exact target before requesting any action.

An identify exchange returns a fresh public local instance ID. The stop and
completion records echo that ID on the same socket. The ID is neither a remote
session ID nor a bearer credential. The client never reconnects or retries an
uncertain stop against a replacement owner.

There are at most four pending local peers, bounded admission work per service
turn, a two-second absolute handshake budget, and a maximum 253-byte UTF-8
target. All records have fixed or checked bounded sizes. A stalled or malformed
caller is retired without cancelling the native session. The service runs beside
the existing connection on Asupersync and does not own input authority.

## What a close result proves

After an authenticated stop request, the original connection supervisor is
cancelled but its operation future remains alive. Its existing input fencing,
any in-flight closing exchange, native worker reap, and file cleanup continue. The endpoint
stays bound while this happens. Only after that original operation completes does
the owner reply with its actual local cleanup outcome.

A successful JSON response has schema version 1, stage `observed`, and data with
`scope: "local_connection"`, the public local `connection_id`, the exact target,
`closed: true`, and `local_cleanup_confirmed: true`.

This confirms the local connection and native cleanup. It does **not** claim
remote key release, successful delivery of earlier actions, or rollback. The
original `fr connect` result retains its host closing reports, control receipts,
transfer outcomes and any unknown effects.

If native cleanup is incomplete, the close command exits nonzero. If the stop
may have been submitted but the caller loses its reply, exceeds its fixed
30-second completion wait, or is interrupted, it reports
`disconnect_outcome_unknown` with outcome `unknown_external_effect`. A missing
reply is never converted into refusal or success, and no automatic retry follows.

## Verification status

Regression coverage is added for actual abstract Unix sockets, bilateral kernel
credentials, duplicate owners, exact target/instance checks, malformed and stalled
callers, bounded handshakes, and reply-after-cleanup ordering. CLI tests cover
closure without LocalAPI, nonexistent owners and refusal of unissued lease
handles. Separate shutdown tests retain the original cleanup future after a local
stop and ensure listener errors do not manufacture a stop request.

These are implementation and test-source claims. Native Cargo execution remains
blocked in this session by disk exhaustion and out-of-memory kills while building
the pinned Asupersync dependency; full application and live-host qualification
are not claimed.
