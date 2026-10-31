# Authenticated native capability inspection

Plan sections 6.4 and 18.1; bead `fr-p2-discovery-sx4`.

`fr inspect NODE_ID` retains its installed-Tailscale metadata-only behavior.
The explicit probe contacts the selected machine's native listener:

```sh
fr inspect NODE_ID --probe --experimental-native --json
fr inspect workstation.example.ts.net --by-name --probe --experimental-native
fr inspect NODE_ID --probe --experimental-native --trust-roots /absolute/ca-roots.pem
```

The local distribution trust bundle is the default. `--port` selects the
listener port and `--ipv6` restricts the native dial to IPv6. Probe-only flags
without `--probe` are rejected. This command runs one bounded inspection;
it does not scan the tailnet or open a desktop after a failed probe.

## Bounded tailnet discovery

An explicit sweep applies the same acknowledged inspection to a bounded set of
machines returned by the installed Tailscale daemon:

```sh
fr hosts --probe --experimental-native --json
fr hosts --probe --experimental-native --probe-limit 12 --port 8443
```

The default selects at most 64 candidates in stable-node-ID order;
`--probe-limit 1..64` lowers that count. The original twenty-second deadline
covers the LocalAPI discovery and all probes. Each probe additionally retains
its own five-second limit, including its four-second dial allowance. At most
four probes run concurrently. A peer's failure or close cannot cancel a
sibling, monopolize a lookup slot, or reset the sweep's deadline. Cancellation
and expiry drop the original pending futures and fence their native owners;
there are no detached probe tasks or automatic retries.

The JSON envelope's `data.scope` is `tailnet_capability_sweep`. It includes
candidate, selected, omitted and exclusion counts, fixed resource/time bounds,
`complete`, `budget_exhausted`, and an ordered `hosts` array. Each row has one
of four states:

| State | Meaning |
| --- | --- |
| `capabilities_observed` | The exact peer acknowledged the pre-observation inspection; `inspection` contains its advertised intersection. |
| `probe_failed` | That peer's original probe ended with a content-free typed error and next action; the other probes continue. |
| `probe_interrupted` | The sweep's original deadline interrupted that peer; no success is invented from a partial handshake. |
| `not_probed` | The deadline ended before that candidate was contacted. |

`complete` means every selected probe returned a result, including refusals;
it does not mean every host is usable. Peers beyond the selection limit are
counted in `omitted`. The original discovery's shared, expired and unusable
exclusions remain visible. A timeout is not evidence that a host is offline or
that FrankenRemote is absent. Successful rows preserve unknown display count,
approval policy and live codec admission. Ordinary `fr hosts` remains a local
metadata lookup, and a later connection always performs fresh authorization.

This is an explicitly requested one-shot sweep. It creates no persistent
capability cache, background network activity, subnet scan or observation.

## What success means

A successful report means the selected host completed canonical native TLS and
Tailscale identity checks, supplied a valid capability intersection, and
acknowledged this inspection's closing exchange. The report uses the existing
robot JSON envelope with `scope: "advertised_capability_intersection"`
and `completion: "inspection_acknowledged"`.

The intersection contains only capabilities understood by both endpoints, with
their explicit versions and required markers. Its seven resource limits are the
values actually transmitted in `HostCapabilities`, reduced by the client's
offer. Host-local admission rates and hardware resource defaults are not
invented as remote measurements.

Observation, input requests, and native-worker startup are all false for this
inspection. Display count and approval policy remain unknown. There are no
pixels, display handles, titles, clipboard previews, audio, bearer tickets or
input authority in the result. The advertised codec/profile names do not prove
that the current OS, driver or hardware passes live encode/decode admission.
Ordinary display inspection and connection retain their own approval and
qualification checks.

The identity and network-path fields originate in installed Tailscale discovery.
They describe that snapshot, not a transport performance measurement.

## Connection and lifetime

The exact discovered stable node ID and canonical certificate name form a
comparison-only pin. Each subsequent LocalAPI resolution checks both values;
TLS still verifies the canonical name and local roots, and the canonical dialer
revalidates the identity after its handshake. A remembered name or address
cannot substitute for current installed-tailnet identity.

The entire native dial and inspection has one five-second budget. The dialer
may use four of those seconds, shared between the verified address families;
each family needs room for the pinned transport's 1.5-second cold-listener
retransmission. This does not add time to the whole operation. Application
startup additionally has a two-second deadline, and the host clamps recognized
inspection work to its original two-second inspection budget. A stalled peer
does not receive a fresh budget. Normal automatic address-family selection may
try its already verified alternative within the same overall deadline.

The client sends an observe-role `ClientHello` with the required
`native-capability-inspection` version 1 marker. An older host refuses
the required capability. The client never downgrades to an ordinary observation.
A supporting host returns the bounded intersection, then accepts only the exact
bootstrap inspection-complete close. A selection request under this marker is
refused before approval or observation can open.

The bootstrap request and reply use zero binding/session identifiers solely for
this required, negotiated inspection mode. Ordinary session closure still
requires a real binding. Malformed headers, lengths, bodies or other zero-bound
session messages are not admitted by the new framing path.

A cold host listener completes and retires the original probe peer, then waits
for another peer with its unconsumed desktop source and display-selection
callbacks. A recognized abandoned probe also retires without consuming those
callbacks. A later genuine viewer follows the existing approval and desktop
startup path. Local share-session revocation and unrelated failures retain their
terminal behavior. The host's existing unlocked-session and OS-permission gates
still apply; this feature does not create a permissionless listener.

## Evidence and remaining work

Regression coverage exercises exact bootstrap encodings and malformed records,
ordinary-close separation, required-marker downgrade refusal, client startup
ordering, cancellation, original deadlines, and repeated probes followed by a
real approved session. Report tests verify the exact seven transmitted limits
and prevent unknown approval, display or hardware state from becoming success
claims.

The sweep scheduler has deterministic tests for four-slot concurrency, stable
result ordering, original-owner cancellation, unpolled abandonment, deadline
crossings and cooperative immediate completions. Command-parser tests enforce
the opt-in, candidate bound and strict options. These tests are not native
network or live-tailnet qualification.

Automatic background probing, expiry-aware capability caching, mobile host-link import and an authenticated
directory remain separate work. Native transport and real hardware
qualification retain their existing status.
