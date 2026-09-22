# Durable local host policy (Linux)

`frd approval get|set local|set none` and `frd sharing get|set own-user|set tailnet`
read and update real local configuration. The default file is
`$XDG_CONFIG_HOME/frankenremote/host-policy.json` (or
`$HOME/.config/frankenremote/host-policy.json`); root uses
`/etc/frankenremote/host-policy.json`. `--config /absolute/path.json` selects
another file for these commands and `frd run`.

For example:

```sh
frd approval set local
frd sharing set own-user
frd approval get --json
frd run
```

These are **saved startup defaults**, not live-session administration. They do
not revoke an existing connection or confirm the state of a running daemon.
Restart to load them, and remove any explicit `--approval` or `--sharing`
arguments in a service unit that should inherit the saved defaults. Explicit
startup flags take precedence and never rewrite the file. JSON output reports
revision, persistence, activation scope, and whether anything actually changed.
No listener or browser qualification is implied; the active listener/session
reactor remains tracked separately by `fr-0il`.

Missing files use the plan's own-user sharing and optional approval-off defaults.
Unreadable, malformed, duplicate-field, unknown-version, oversized, symlinked,
non-private, or non-regular files refuse rather than falling back to those
defaults. Files are private to the local account; directory-relative I/O and an
atomic rename prevent partial reads. A stable nonblocking writer lock preserves
concurrent field updates. A failed directory sync after publication is reported
as uncertain crash durability, not as an unapplied save. Invalid, unknown,
duplicate, or missing startup option values refuse before contacting Tailscale.
