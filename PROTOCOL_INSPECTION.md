# Terminal native capability inspection (v1)

This extension implements the bounded, authenticated capability-probe part of
plan §6.4. It reports the common protocol capability intersection and negotiated
resource ceilings. It does not observe the desktop or test native hardware.
The required capability is `native-capability-inspection`, version 1, within
native profile 1/version 0 and application version 0.

## Negotiation and authority

The caller prepares an Observe offer with the marker required. Preparation
refuses RequestControl intent instead of silently downgrading it. At most 23
other capabilities fit beside the marker within the existing 24-entry bound.
Older hosts that do not implement the marker must refuse the required capability;
there is no fallback to a selected or observing session.

After authenticated native TLS/ALPN and the existing Tailscale admission, the
client sends ClientHello. The host validates the offer and intersects the normal
capabilities and limits, then includes the required marker in HostCapabilities.
The diagnostic marker is handled by the terminal inspection implementation;
normal session offers do not need an extra capability slot. All required
capabilities on either side still have to match exactly.

The only next client record is the fixed bootstrap CloseRequest below.
SelectedConfiguration, approval requests, SessionOpened, BindingAccepted,
attachments, and all observation/control operations are invalid in this mode.
The host fences the unselected authority owner before reporting the fixed Closed.
No native source, worker, display discovery, approval notification, or input
effect is created. The client returns capabilities only after validating Closed.

HostCapabilities is the validated **intersection**, not a full host inventory.
An omitted optional capability might be unknown to this client. Advertised
capabilities are protocol implementation claims, not current device permission,
HEVC performance, hardware readiness, display inventory, or audio activity.
Local approval policy and display count remain unknown in this exchange.

## Fixed bootstrap terminal records

These are two exact forms on the original reliable bootstrap control pair,
available only after both offers negotiate the required marker. They retain the
existing kind numbers. Every byte, including the empty extension area and the
zero identifier, is checked. A direction mismatch is rejected by the startup
state machine.

Both use the ordinary 24-byte FRD0 header: application version 0, flags/reserved
zero, binding 0, extension length 0. Integers are big-endian.

| Direction and kind | Exact payload | Total bytes |
|---|---|---:|
| Viewer → host, CloseRequest 0x001d | session:16 zero bytes; scope:u8=1; reason:u16=4 (InspectionComplete) | 43 |
| Host → viewer, Closed 0x001e | session:16 zero bytes; reason:u16=1 (ClientRequested); cleanup:u8=2 (Complete); effects_known:u8=1; pending:u32=0; uncertain:u32=0 | 52 |

The zero session is an absent-session sentinel only in these exact forms. It is
not a wildcard or a usable authority binding. The ordinary session-close codec
and generic Record decoder continue rejecting zero-bound CloseRequest/Closed.
The negotiation framer accepts only the two fixed payload lengths with no
extensions; the transport family then validates the exact bytes before dispatch.
This adds no generic zero-binding message route.

Complete cleanup and known zero effects are valid here because no session
authorization, native operation, or external effect was admitted. They are not
inferred for ordinary session teardown and cannot be used to report that an
input action was rolled back.

## Bounds, cancellation, and listener ownership

Each endpoint caps the probe at two seconds from its original startup owner's
creation, including parked time. Receiving bytes, repeated polling, and preparing
the terminal exchange cannot restart this deadline. The existing offer/record
bounds and Tailscale lifetime checks remain in force.

After fencing, the host may drain the single fixed acknowledgement for at most
100 ms, also bounded by the original probe and admission deadlines. It uses the
same transport and pending-record deadlines, performs no admission refresh,
and dispatches no application receive in that terminal scope. A successful
local flush is not evidence of peer receipt; only the client's validated Closed
allows a successful capability result.

The owning Viewer is captured when inspect_capabilities is called, even if the
returned future is never polled. Drop and errors cancel that original owner.
There is no application-close API assumption: an abruptly abandoned recognized
probe on the host ends under its original bounded deadline.

A recognized probe has a distinct host terminal outcome: InspectionComplete
after the fixed terminal send succeeds locally, or InspectionAbandoned for a
subsequent peer refusal, timeout, or failed terminal send. Explicit local clock,
configuration, allocation, source-lifetime and owner-cancellation faults retain
their original error and do not authorize a cold retry. Both retire
the peer before the desktop dispatcher accepts the next cold connection. They
do not consume its native source factory, display-selection callback, or local
approval callback. The independent OS-share lifetime must still be valid before
the dispatcher rearms. No failed probe can revive an expired share owner.
