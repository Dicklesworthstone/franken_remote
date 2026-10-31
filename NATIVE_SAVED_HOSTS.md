# Saved native hosts with stable identity pins

Plan sections 6.4 and 18.1; bead `fr-p2-discovery-sx4`.

Save a known machine under an explicit local alias, then use that alias for
native connection and approved display discovery:

```sh
fr hosts --json
fr hosts save workstation NODE_ID --json
fr hosts saved --json
fr displays workstation --saved --experimental-native --json
fr connect workstation --saved --view-only --experimental-native --display only
fr hosts forget workstation --json
```

A custom port is selected when saving: `fr hosts save workstation NODE_ID --port 9443`.
The alias is explicit: `--saved` is accepted by ordinary `connect` and
`displays`. It cannot be combined with `--by-name` or `--port`.
An unknown saved alias refuses instead of falling back to live hostname or node-ID
selection. Existing commands without `--saved` keep their current behavior.

Aliases are one to 64 lowercase ASCII letters, digits, dots, underscores or
hyphens, beginning with a letter or digit. Profiles contain exactly the alias,
stable Tailscale node ID, canonical certificate name and service port. A duplicate
alias or endpoint refuses; replacing a profile requires an explicit forget
followed by a fresh save.

## Fresh identity on every connection

Save performs the installed daemon's existing credential-checked, two-read peer
lookup. It contacts no remote desktop. Listing and forgetting are local
operations; they need no running Tailscale daemon and create no async runtime.

A saved selection checks the stable node ID and certificate name together during
both reads of every new LocalAPI resolution. The existing native dialer still
uses fresh local and peer addresses, strict certificate/hostname verification,
post-handshake revalidation and lifetime renewal. Reconnect retains the same
saved identity pin. A renamed or replaced node is refused, including replacement
by another node using the old name.

The profile stores no IP addresses, trust-root paths, permissions, role, approval
choice, media selection, tickets, leases or credentials. Native transport opt-in,
the explicit view/control role, display choice, host approval and current
authorization are all still required. Address changes can be discovered
normally; a changed identity must be consciously saved again.

Local session-control endpoints use the resolved stable node ID. Use that ID
from `fr hosts saved --json` with `fr disconnect NODE_ID` or
`fr robot session close NODE_ID`. Robot commands do not silently adopt saved
alias mode.

## Bounded local storage

The default store is `$XDG_CONFIG_HOME/franken_remote/hosts.json`, or
`$HOME/.config/franken_remote/hosts.json` when XDG configuration is unset.
The `--hosts-file /absolute/path/hosts.json` option selects another local
store for saved-host commands and `connect`/`displays` in saved mode.

The versioned JSON document is limited to 100 profiles and 128 KiB. Loading
validates every record, duplicate alias/endpoint and schema field. Unknown
fields, malformed JSON, noncanonical names and invalid identifiers refuse;
invalid existing contents are never replaced automatically.

On Linux, handle-relative traversal refuses symlinks and untrusted writable
ancestors. Existing files must be private, owned by the current user, regular
and singly linked. Writes use a nonblocking local lock, a new private staging
file, file synchronization and atomic rename. Readers see a complete revision.
JSON results distinguish publication from confirmed crash durability; a failed
directory sync reports `durable: false` after a known publication.

This is local selection metadata, not a pairing database, capability cache,
mobile host-link importer or remotely served directory. The profile creates no
additional network identity or permission.

## Verification scope

The shared model has bounds, schema, duplicate and validation regressions. File
tests exercise actual private files, lock contention, atomic replacement,
malformed-state preservation, symlinks, hard links, special files and unsafe
permissions. Parser and selection tests keep saved mode explicit and preserve
the canonical role/approval choices. Actual CLI integration tests use the
repository's credential-checked LocalAPI fixture.

Fixture identity and filesystem tests do not qualify installed Tailscale
versions, live tailnet connectivity, native hardware or non-Linux platforms.
