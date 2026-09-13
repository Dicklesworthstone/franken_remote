# Acquiring host control while capture continues

`StreamingHost::serve_accepting_control` integrates the existing initial-grant
broker into the original continuous capture and session service. It pairs with
`StreamingViewer::serve_requesting_control`: neither endpoint must stop or
reconstruct its media owner to acquire control. The shared Seat, completed input
attachment, local consent/readiness and independently driven native input owner
remain explicit prerequisites. This is a library integration, not a desktop UI.

## Local application contract

Start this operation instead of `serve` on a configured, unserved StreamingHost.
Provide the OS share-session's existing Seat, the completed NegotiatedInput,
local-control callback, unpredictable observation nonce and renewed-ticket
sources, and bounded dispatch for other application records. Input attachment
must refer to the exact original connection and capture configuration. An
observation-only selection is not silently upgraded, and no attachment or clock
is installed by this operation.

The local-control callback receives `HostControlState::Pending` before the grant.
Its PendingHostControl exposes the validated request and native status, explicit
approve/deny/stop, and no raw connection or mutable broker. Returning a target is
NOT approval. On approval the canonical broker reserves the shared Seat before
minting credentials, checks already-established host readiness, and runs the
native factory on its existing native thread. Poll the returned Driver in an
independent authority task immediately; it must remain driven during initialization,
codec stalls, network failures and cleanup. Do not poll it from a media callback.

Return the CURRENT qualified Target on every local callback. Once promoted,
`HostControlState::Active` supplies the original request and revoke-only native
Control. Missing/changed local target or a callback failure fences native input.
Permission is checked before draining queued input, including during a pending
installed-Tailscale admission refresh. The native owner still checks authority,
expiry and platform state immediately before every OS submission. Native UI/OS
work must not block this callback. Local revoke and lifecycle events should also
signal the independently usable controls immediately, not await a network turn.

## Continuity and failure

The initial request is dispatched on the same bounded ordered control lane as
observation renewal and the session-owned clock. The existing GrantBroker owns
wire parsing, replay floor, Seat reservation and the one bounded grant record.
No second parser, input executor, permission state machine or runtime is added.

The broker retains its original request/ticket/lease deadlines through native
startup and send backpressure. The viewer separately retains its earlier
call-time deadline. A successful transport enqueue is not proof of receipt.
Input arriving immediately after grant enqueue remains transport-owned until the
completed network turn transfers that exact native owner into ControlledHost.

The original capture process, source, packetizer, reference cache, feedback and
adaptive capture state remain in place. After promotion the existing controlled
host services results, held-state reconciliation, control renewal and fresh input
tickets; native submissions still wake the existing capture scheduler.

Dropping even an unpolled operation revokes observation/input before dropping
foreign work. Failed local policy, native startup, transport or source work is
terminal for this session. The native finalizer, not a network or codec task,
releases the Seat after cleanup. `collect_after_close` retains real native
receipts; worker reaping remains explicit with a separate live cleanup context.

## Verification scope

The seven new integration cases exercise production UDP/TLS, negotiated channels,
clock/observation/control renewal, the real grant broker and input owner, and two
supervised media processes. They cover immediate/delayed consent, continuous idle
source verification, refusal without host readiness, no-consent expiry, native
initialization failure, target loss after grant and unpolled cancellation.
Decoder/encoder replies, source checks, readiness, consent, visibility and the
counted OS input sink are explicit fixtures, not physical or hardware evidence.

Local verification on nightly-2026-08-31 rebuilt current first-party sources with
compiler-matched pinned Asupersync 0.5.0 artifacts. All 177 daemon unit tests
passed with four threads; 16 existing native/namespace cases remained ignored.
The new seven cases also passed serially. Strict daemon library/test Clippy and
changed-source formatting passed. This artifact-assisted rebuild is not a cold
full-dependency/native-workspace build; committed-source CI is a separate result.

```sh
cargo test -p frd --lib session_startup::running::streaming::acquisition --locked -- --test-threads=1
cargo test -p frd --lib --locked -- --test-threads=4
./scripts/verify.sh fast
./scripts/verify.sh docs
```

This advances fr-p1-frd-broker-b9n and fr-p1-input-pipeline-ay1 without closing
their broader acceptance. Complete daemon listeners, the desktop consent/visibility
UI, platform qualification and controlled convenience bootstrap remain separate.
