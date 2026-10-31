# Bounded input on the original foreground owner

The Linux native desktop CLI can explicitly accept bounded robot input on its
original authenticated control session. It supports a click, a bounded drag,
a physical key tap, a modifier shortcut, or integer line scrolling. The owner retains the host lease, input ticket,
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
| `--drag BUTTON,X1,Y1,X2,Y2` | Press at the source, move once while held, then release at the destination. Both distinct endpoints must be inside the original granted display. Uses the same button numbers as click. |
| `--key USB_USAGE` | Press and release one validated USB keyboard page-7 physical usage, in decimal; for example `4`. This is not Unicode or a keyboard-layout translation. |
| `--chord MODIFIER,...,KEY` | Hold one to four distinct USB modifiers (224–231) through one physical key tap, then release in reverse order. For example, `224,225,19` requests Control–Shift–P. |
| `--scroll X,Y,DX,DY` | Scroll at the host-desktop point by integer lines on each axis. Each delta is between −120 and 120; at least one is nonzero. |

The age bound must be 1–1000 milliseconds. Existing host and viewer limits may
be stricter. Coordinates may be negative on desktops with a negative display
origin; the original input encoder still checks the selected display bounds.
The owner converts scroll lines to the existing fixed-point wire units. The
caller must already know its target host coordinates; this status response
does not provide a screenshot or a local-to-host coordinate transform. An
explicit [live screenshot](docs/native-client-cli.md#screenshots-from-an-existing-foreground-owner)
can return pixels and the pictured frame's original input handles. It does not
make a later input request atomic with the observation or refresh that evidence.

Clicks, drags and key taps contain two reliable actions. Modifier shortcuts
contain at most ten. Each next action is admitted only after the original host
reports successful submission of the preceding action and the original
observation, generations, focus and authority remain valid. There is one pending
request and at most eight retained fixed-size reports. No persistent held keys,
arbitrary batches, committed text,
relative pointer, pixel scrolling or pointer replay are exposed.

### Drag with the original observation

Use a fresh ready observation from the same owner and its supplied next request
ID. For example, a primary-button drag between two signed host-desktop points:

```bash
fr robot input NODE_ID --connection-id CONNECTION_ID \
  --lease LOCAL_LEASE --request-id NEXT_REQUEST_ID --observation-id OBSERVATION_ID \
  --max-observation-age-ms 1000 --drag 1,-100,20,200,40 --json
```

Both endpoints are checked before the press can be encoded. An outside point
returns `robot_input_out_of_bounds`; coordinates are never clamped. A changed
display bound or generation cannot reinterpret an already accepted gesture.
The two reliable button records carry their own absolute-position barriers:
the destination motion happens while the button is still held, before release.
This is one bounded move, without a duration, interpolated path or persistent
held-button mode. A stationary drag is refused; use a click for that operation.

The release keeps the press's original observation and deadline. Expiry,
cancellation or focus loss after a submitted press retains the partial or
unknown result and fences the original owner. Existing held-state cleanup can
release the button, but does not undo a window move, selection or other effect.
Changing either endpoint under an existing request ID cannot execute a new drag.

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

The combined CLI validation scope passes 102 actual-source tests, covering saved
hosts and command parsing, input action mapping, the local owner and request
ledger, and control-screenshot observation pins. It passes strict Clippy. The
tests use the real client/core/wire types and exercise endpoint bounds,
immutable request fingerprints, original receipts, partial effects, cancellation
and deadline retirement. The scoped harness omits socket I/O and the native
`ControlledViewer` dispatcher; its unused control-handle boundary is
uninhabited. These results establish the tested parser/state behavior, not a
complete native network run.

Two actual X11 tests pass under private Xvfb: the existing
`real_pointer_drag_barriers_duplicates_and_expiry_cleanup` and
`two_reliable_buttons_move_while_held_before_release_at_the_drag_destination`.
The latter observes server state after each production input submission:
source/button-up, source/button-down, destination/button-down, then
destination/button-up. Exact duplicate records return their retained receipts
without additional OS submissions. Strict Clippy also passes for this native
test scope. These checks verify native input ordering and cleanup; they do not
establish application-level drag/drop success.

The complete foreground CLI-to-host regression is wired into the existing
namespace verification lane. Full native Cargo validation and real two-machine
qualification remain separate gates. Application-semantic assertions and an
atomic observe/act transaction remain separate work.
