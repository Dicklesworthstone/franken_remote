# Durable local host policy (Linux)

`frd approval get|set local|set none` and `frd sharing get|set own-user|set tailnet`
read and update real local configuration. The default file is
`$XDG_CONFIG_HOME/frankenremote/host-policy.json` (or
`$HOME/.config/frankenremote/host-policy.json`); root uses
`/etc/frankenremote/host-policy.json`. `--config /absolute/path.json` selects
another file for these commands and `frd run`.

For example:

```sh
frd approval set none
frd sharing set own-user
frd approval get --json
frd run --software-explicit
```

`frd run` currently refuses a saved `local` approval mode
(`local_approval_unavailable`: the interactive session-agent process that would
show the prompt does not exist yet), and it refuses without
`--software-explicit` (`hardware_hevc_unavailable`: no hardware encoder
selection yet; see [ADR 0004](docs/decisions/0004-software-encoder-profile.md)).

`frd run` watches the **same resolved policy file** throughout its lifetime.
An observed new revision fences existing grants and retires the old source,
transport and ingress before accepting a new share. Changing approval to `local`
stops an active unattended share; the current host then refuses with
`local_approval_unavailable` instead of silently continuing without a prompt.
Malformed or stale policy evidence also stops sharing rather than falling back.

Explicit `--approval` and `--sharing` flags override saved values for that process,
not revision fencing. Even a revision whose effective settings are unchanged
ends old grants. Remove these flags from a service unit to inherit saved values.
The management commands publish the file but do not receive an application
acknowledgement from a running daemon: `live_application` is `unconfirmed`,
`applied_to_running_daemon` and `restart_required` are JSON null. A successful
save is not proof that a particular process is watching the same path or has
finished cleanup. Startup-only library callers still load on their next start.
See [live-policy ownership and bounds](LIVE_HOST_POLICY.md).

Missing files use the plan's own-user sharing and optional approval-off defaults.
Unreadable, malformed, duplicate-field, unknown-version, oversized, symlinked,
non-private, or non-regular files refuse rather than falling back to those
defaults. Files are private to the local account; directory-relative I/O and an
atomic rename prevent partial reads. A stable nonblocking writer lock preserves
concurrent field updates. A failed directory sync after publication is reported
as uncertain crash durability, not as an unapplied save. Invalid, unknown,
duplicate, or missing startup option values refuse before contacting Tailscale.

## Installing a service with these settings

`frd install` leaves omitted `--approval` and `--sharing` flags out of the unit,
so the daemon inherits the saved policy at startup and on later revisions. Explicit values are retained,
including `--approval none` and `--sharing own-user`; they never disappear merely
because they match the original plan defaults. `--config /absolute/path.json`
is preserved in Linux service definitions. The file must be readable by the
service account, not just the account invoking the installer.

`frd install --dry-run` now prints the actual generated definition. JSON preview
output includes `unit_content` and `next_steps`, with `started: false`. Writing a
unit is not evidence that the daemon is running. Invalid/duplicate flags, invalid
ports and unsafe argument encodings refuse before installation. Service paths
are encoded for systemd or XML rather than interpolated as commands/markup.
