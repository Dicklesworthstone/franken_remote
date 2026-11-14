# Windows client

`fr-windows` uses the installed Tailscale service to find a host and revalidate
its identity before opening a native viewer or fetching its directory. The
viewer uses Media Foundation decode and D3D11 presentation. The same session
core serves the iOS app, whose named connection path remains available through
its existing C interface.

Build on Windows with the repository's pinned Rust toolchain:

```powershell
cargo build --locked --release -p fr-windows
```

Run the resulting executable in the interactive desktop session. Tailscale
must be installed, signed in and running on this machine. The remote host must
be running FrankenRemote and its own admission policy must allow this viewer.

## Find and select a host

```text
fr-windows hosts
fr-windows view <stable_id-or-full-name>
fr-windows directory <stable_id-or-full-name>
```

`hosts` reads one bounded snapshot from the installed service's authenticated
LocalAPI. Its JSON document contains:

- `machines`: each eligible peer's `stable_id`, canonical `name`, current
  Tailscale `addresses`, and sampled `path` (`direct`, `relayed` or `unknown`).
- `readiness: "not_probed"`: listing a machine says nothing about whether
  FrankenRemote is running or whether its desktop is available.
- `excluded`: counts of shared, expired and otherwise unusable peers.

No remote probes run during this command. An empty list is a valid result.
Select one exact stable ID or a full name from the listing. Full names accept
ASCII case differences and a trailing DNS dot; short hostnames, URLs and
ambiguous choices are refused.

Each `view` or `directory` command reads its own current snapshot and retains
the selected stable ID and certificate name. The connection then resolves both
again through the installed daemon. An unavailable daemon or refused selection
ends the command with an explanatory, content-free error.

## View, control and choose an address family

```text
fr-windows view remote.example.ts.net
fr-windows view remote.example.ts.net --control
fr-windows view remote.example.ts.net --ipv6
fr-windows view n-selected --ipv4 --port 8443 --seconds 30
```

Replace the sample name or ID with a value from your own listing. The default
port is 8443. Without a family flag, the existing native connector prefers an
authenticated IPv6 route and can try its authenticated IPv4 alternative before
application startup. This works when Windows MagicDNS answers only an A record
but the host serves IPv6: the installed metadata supplies the node's current
address families.

`--ipv4` and `--ipv6` are exclusive choices. A missing selected family refuses
the connection. These flags never add an address absent from the authenticated
metadata.

`view` starts an observing session. `--control` requests control through the
existing host admission and grant flow. `--seconds N` requests an orderly close
after a positive number of seconds. Closing the window also closes the session.
The viewer prints its existing JSON completion document with counts, timings
and the typed ending.

## Explicit address constraints

`--address IP` remains repeatable, up to eight distinct addresses. It now
constrains the host's **complete current authenticated address set**. The order
does not matter. A subset, duplicate, additional address or different node
address refuses the command.

For a node listed with both of these addresses:

```text
fr-windows view remote.example.ts.net --address 100.64.0.2 --address fd7a:115c:a1e0::2
```

Supplying only the IPv4 address would refuse. Use `--ipv4` to choose that family
without supplying an explicit address set. A complete address constraint may
also be combined with a family flag.

The constraint is checked against the selection snapshot and both fresh
metadata reads used to authorize the connection. It never replaces
authenticated metadata or turns the command into a direct IP or DNS fallback.
Reconnection and live renewal retain the connector's original identity
checks: changed names, addresses, keys, membership or local daemon identity
stop the attempt instead of silently selecting a replacement.

TLS still verifies the selected canonical certificate name and chain against
the bundled ISRG trust roots. Discovery alone grants no observation, input,
clipboard, audio or file authority.

## Fetch a host's directory

```text
fr-windows directory remote.example.ts.net
fr-windows directory n-selected --ipv6 --port 8443
```

The directory command uses the same installed identity path as the viewer.
It performs one bounded fetch, prints the shared directory JSON document, and
exits successfully only when the host returned a listing. The host must enable
directory service; its refusal remains visible in the result.

## Local capture probe

```text
fr-windows capture-probe --seconds 10 --fps 30
```

The existing capture probe remains independent of host discovery. It exercises
the local Desktop Duplication, hardware HEVC encode and decode path in the
interactive session and prints counts and timings.

## Validation scope

Portable command-line regressions live in
`crates/fr-windows/src/options.rs`. Installed target selection and named-route
compatibility regressions live in
`crates/fr-mobile-ffi/src/session/connection/tests.rs`. The exact address
selector's metadata, fresh-read and renewal regressions live in `fr-tailnet`.

Those checks cover selection and route policy. They do not establish native
Windows pipe access, GPU behavior or live tailnet connectivity on a particular
machine; native platform qualification remains necessary.
