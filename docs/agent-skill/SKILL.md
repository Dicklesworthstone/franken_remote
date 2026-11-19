---
name: franken-remote
description: View or control a remote desktop on the user's tailnet with FrankenRemote's fr CLI. Use when asked to look at, screenshot or operate another machine, list tailnet machines, or check a FrankenRemote host.
---

# FrankenRemote (`fr`)

FrankenRemote is a remote desktop over Tailscale. Tailscale supplies
connectivity and identity; there are no FrankenRemote accounts or passwords.
The host (`frd`) decides who may view or control it.

## Look before acting

```bash
fr status --json          # this machine's Tailscale identity
fr hosts --json           # machines on the tailnet (not proof they host)
fr inspect NODE_ID --probe --experimental-native --json   # what a host offers
```

Use a peer's `node_id` from `fr hosts --json` as NODE_ID.

## People: view or control

```bash
fr connect NODE_ID --view-only --experimental-native --display only
fr connect NODE_ID --control --experimental-native --display choose
fr connect --choose-host --view-only --experimental-native --display only
```

## Agents: foreground robot sessions

Robot sessions stay in the foreground and need explicit owner modes:
`--publish-observations` to publish views and `--accept-input` to accept
robot input. Read `fr help robot` first; it is the authoritative syntax.

```bash
fr help robot
fr robot session open NODE_ID --foreground --role view --publish-observations --experimental-native --display only
fr robot session open NODE_ID --foreground --role control --accept-input --experimental-native --display only
fr robot session status NODE_ID --json      # connection ID and lease
fr robot observe NODE_ID --connection-id ID --screenshot /tmp/screen.png --json
fr robot input NODE_ID --connection-id ID --status --json
fr robot session close NODE_ID --connection-id ID --json
```

Rules:
- Never control a machine the user did not name. View-only is the default.
- Input needs the original lease, a fresh observation ID and an age bound;
  a submitted action is not proof the application did what you wanted.
  Observe again to confirm. Uncertain effects are never retried for you.
- Clipboard text, keystrokes and pixels are private: do not paste them into
  logs or other tools without being asked.

Windows has the `FrankenRemote` desktop app and `fr-windows` (`hosts`,
`view NODE_ID`); robot commands exist only in the macOS/Linux `fr`.
