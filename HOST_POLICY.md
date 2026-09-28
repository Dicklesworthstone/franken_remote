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

`frd install` requires `--software-explicit` for Linux systemd units and refuses `--approval local` (typed `hardware_hevc_unavailable` / `local_approval_unavailable`), because `frd run` would refuse both at every restart.
`frd install` leaves omitted `--approval` and `--sharing` flags out of the unit,
so the daemon inherits the saved policy at startup and on later revisions. Explicit values are retained,
including `--approval none` and `--sharing own-user`; they never disappear merely
because they match the original plan defaults. `--config /absolute/path.json`
is preserved in Linux service definitions. The file must be readable by the
service account, not just the account invoking the installer.

Further `frd run` options go after `--` and land in the unit's `ExecStart`, for
example `frd install --user --software-explicit --approval none -- --input-agent
/usr/libexec/fr-input-agent --clipboard --logind-session c2`. They are parsed by
`frd run`'s own parser at install time and refused when every start would refuse
them: unknown or valueless flags, relative paths, `--clipboard`/`--files` without
`--input-agent` (typed `clipboard_requires_input_agent` /
`files_requires_input_agent`), flags the installer sets itself (`--port`,
`--socket`, `--config`, `--approval`, `--sharing`, `--software-explicit`), and
`--once`. A **system** unit has no user's X display, so it is refused
(`system_service_requires_headless`) unless it shares a private `--headless`
Xvfb. Only systemd units carry these options.

A user unit is wanted by `graphical-session.target` (which provides `DISPLAY`),
not `default.target`, which SSH-only logins also reach. Units use
`Restart=on-failure` with `RestartPreventExitStatus=2`: `frd run`'s configuration
refusals exit 2 and are not retried in a loop; runtime failures are. `frd
uninstall` removes the unit file and prints how to stop and disable a service that
is still running (it does not stop it itself, just as install does not start it).

`frd install --dry-run` now prints the actual generated definition. JSON preview
output includes `unit_content` and `next_steps`, with `started: false`. Writing a
unit is not evidence that the daemon is running.

A **user** unit runs `frd run` without `CAP_NET_ADMIN`, so it cannot install its
nftables ingress rule. It asks the root `frd ingress-helper` instead, and refuses
with `ingress_helper_unavailable` until that helper runs. `frd install --user`
therefore also renders the helper's system unit
(`/etc/systemd/system/frd-ingress-helper.service`) and its root-owned
configuration (`/etc/frankenremote/ingress-helper.json`, admitting the installing
uid). It never writes either one, because both need root. Human output prints
both files, and JSON output includes them under `ingress_helper` with
`requires_root: true` and `written: false`. Each next step is labelled with the
account it needs:

- `[user]`: reload and enable the user unit;
- `[root]`: copy `frd` to a root-only path when the installing binary is
  user-writable, write the config and unit, then `systemctl enable --now
  frd-ingress-helper`.

A `--system` unit runs as root and installs its rule directly; no helper is
rendered. The helper's API, bounds and residual trust are in
[LINUX_NATIVE_INGRESS.md](LINUX_NATIVE_INGRESS.md). The rendered unit has not been
exercised under a live systemd. Invalid/duplicate flags, invalid
ports and unsafe argument encodings refuse before installation. Service paths
are encoded for systemd or XML rather than interpolated as commands/markup.
