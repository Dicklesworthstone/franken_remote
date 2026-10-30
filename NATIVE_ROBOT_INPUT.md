# Bounded input on the original foreground owner

The Linux native desktop CLI can explicitly accept bounded robot input on its
original authenticated control session. It supports a click, a physical key tap,
or integer line scrolling. The owner retains the host lease, input ticket,
original viewport and reliable transport; local commands receive comparison
handles and content-free results.

This is an experimental native slice of plan §18, with the authority and
freshness rules of §§6–7 and §19. Its source and qualification status are listed
below. A compositor receipt is not optical visibility or proof that an
application accepted an action.

## Open and address the owner

Build `fr` with `--features linux-desktop` and use the existing qualified native
host profile with an input agent. The canonical native connection options still
control trust roots, Tailscale LocalAPI selection, worker selection and display
selection. Start the owner in a foreground terminal:

```bash
fr robot session open NODE_ID --foreground --role control --accept-input \
  --experimental-native --display only --json
```

Focus that viewer window. From another terminal as the same operating-system
user, obtain its public `connection_id` and then an input observation:

```bash
fr robot session status NODE_ID --json
fr robot input NODE_ID --connection-id CONNECTION_ID --status --json
```

Use the exact original target spelling. These local commands authenticate both
Unix-socket peers, compare the original invocation ID and never open a remote
connection. An ordinary `fr connect`, a read-only observation publisher, or a
foreground session without `--accept-input` does not accept these actions.

A ready input observation supplies `lease`, `observation_id`,
`next_request_id`, all four input generations, a frame identity and
`source_age_upper_ms`. The lease is an opaque local comparison handle, not a
host bearer ticket. Missing or expired evidence returns `ready: false` with
unknown fields left null. Source age includes the original clock uncertainty
and conservative local elapsed time, including the status round trip.

## Submit one bounded operation

Use the values from the same owner's ready status. The following example is a
primary-button click at signed host-desktop coordinates `(640, 360)`:

```bash
fr robot input NODE_ID --connection-id CONNECTION_ID \
  --lease LOCAL_LEASE --request-id 1 --observation-id OBSERVATION_ID \
  --max-observation-age-ms 250 --click 1,640,360 --json
```

Choose `next_request_id` rather than assuming it is 1. The allowed operation
forms are:

| Option | Meaning |
| --- | --- |
| `--click BUTTON,X,Y` | Press and release at the same host-desktop point. Buttons: 1 primary, 2 secondary, 3 middle, 4 back, 5 forward. |
| `--key USB_USAGE` | Press and release one validated USB keyboard page-7 physical usage, in decimal; for example `4`. This is not Unicode or a keyboard-layout translation. |
| `--scroll X,Y,DX,DY` | Scroll at the host-desktop point by integer lines on each axis. Each delta is between −120 and 120; at least one is nonzero. |

The age bound must be 1–1000 milliseconds. Existing host and viewer limits may
be stricter. Coordinates may be negative on desktops with a negative display
origin; the original input encoder still checks the selected display bounds.
The owner converts scroll lines to the existing fixed-point wire units. The
caller must already know its target host coordinates; this status response
does not provide a screenshot or a local-to-host coordinate transform. The
read-only observation publisher is not attached to this automation owner.

Clicks and key taps contain two reliable actions. The release is admitted only
after the original host reports successful submission of the press and the
original observation, generations, focus and authority remain valid. There is
one pending request, at most two encoded actions, and at most eight retained
fixed-size reports. No persistent held keys, arbitrary batches, committed text,
relative pointer, pixel scrolling or pointer replay are exposed.

## Identity, expiry and cancellation

Every accepted request fixes its complete arguments, observation identity,
source-age bound and original deadline. Reading a newer status does not refresh
an accepted request. A newer status replaces an unused observation pin; callers
must coordinate their status reads instead of racing another local controller.

Use increasing nonzero request IDs. A valid request refused because the owner
is busy or its observation is stale still consumes its ID. Repeating the exact
request returns retained evidence or waits for the same pending request; it
never re-encodes it. Changing any argument under the same ID is refused. Once
a report is evicted, its consumed ID still cannot execute again.

The original local peer has a two-second absolute service budget; an accepted
request also has a two-second deadline that cannot renew. Dropping or timing out
the owner's pending request future fences unfinished input and the original
control owner. A caller losing its connection does not prove immediate
cancellation; the original bounds still apply and any unconfirmed effect stays
unknown.
Completed evidence is retained while that foreground owner remains available.
After the owner exits, its results are no longer queryable; a replacement owner
cannot answer for it. An unavailable result remains unknown, not safe to replay.

The native event worker checks focus on the exact original viewer drawable
without creating an input-capture thread or grab. Synthetic FocusIn cannot grant
readiness. After initial focus, loss is terminal, even if focus returns within
the same native event batch. A stalled query, unmap, resize, local close, host
revocation or authority expiry also prevents further input. This mode has one
connection attempt, no clipboard/files/audio, and does not forward human
keyboard or mouse events. Existing native cleanup releases held state through
the original control session; it does not undo an application action.

## Results

| `outcome` | Evidence and next step |
| --- | --- |
| `success` | All actions have matching successful host receipts. Stage describes OS submission or the host's stated observation; it does not prove application success. |
| `partial_submission` | A known prefix reached the OS but the complete operation did not. The original control owner is fenced. |
| `unknown_external_effect` | An encoded action lacks a definitive receipt or the host reports uncertainty. The original owner is fenced; do not replay with a new ID. |
| `refusal` | No external submission is known. The response retains the applicable local or host reason and request counts. |

Reports expose encoded, acknowledged and submitted action counts, submitted OS
operation counts, stage, terminal reason and uncertainty. They do not expose
host ticket credentials, request fingerprints, text, coordinates or key content.
A local reply must match both the original request ID and its full action count.
A malformed or lost reply after request submission is an unknown effect.

Close the original foreground owner through the existing local lifecycle path:

```bash
fr robot session close NODE_ID --connection-id CONNECTION_ID --json
```

## Verification boundary

The `fr-client` request ledger and fail-closed precondition tests pass (77 library
tests in the targeted client suite), as does that library's strict Clippy check.
The production native focus gate passes three Rust tests; six direct production
C ABI regressions pass against a real private Xvfb server and independent Xlib
peer. The desktop C sources compile with `-Wall -Wextra -Werror`.

The 12 actual parser/render tests and 14 actual local-owner state tests pass
in scoped, same-source unit harnesses using the real client/core/wire types.
Those harnesses omit the native entry point, IPC and executor runtime; the
state harness uses an unused stop-handle adapter. They establish the tested
parser/state behavior, not a complete native build. The native end-to-end
regression is wired into the existing namespace verification lane. Full native Cargo execution and real two-machine
qualification of this new automation mode remain unqualified until their
respective gates pass. This mode supplies input-readiness metadata; screenshot
capture, application-semantic assertions and an atomic observe/act transaction
remain separate work.
