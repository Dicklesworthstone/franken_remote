# iOS release qualification

The shipping target is `FrankenRemote/FrankenRemote.xcodeproj`, scheme
`FrankenRemote`. The sibling Swift packages are not substitutes for this app.
Plan sections 16.2 and 20.2–20.3 own the native UI and packaging requirements.

The app uses bundle ID `dev.frankenremote.ios`, team `AU8V2Z6NKY`, and App Store
Connect app `6834614644`. The initial paid version is 1.0 at USD 4.99.
`ExportOptions.plist` exports an App Store Connect IPA without uploading it or
silently changing its version/build number. Signing credentials remain outside
the repository.

## Resources and dependency resolution

`App` is an Xcode filesystem-synchronized group belonging to the app target.
It includes `Assets.xcassets` and `PrivacyInfo.xcprivacy`; verify both in the
resulting bundle, not just in the source tree. The icon is an opaque 1024-pixel
PNG. Xcode supplies the platform mask.

The Rust build phase uses `Cargo.lock` with `--locked` and defaults to two
Cargo jobs. `FR_CARGO_JOBS` changes the bound. `FR_CARGO_TARGET_DIR` selects an
isolated output directory. Keep large outputs on the configured build volume.
`FR_CARGO_CONFIG` can override dependencies for development; a release receipt
must record any such override and must not imply that a patched build matches
an unmodified source revision.

## Privacy audit

The manifest currently declares these source-verified uses:

| API category | Reason | Use |
| --- | --- | --- |
| User defaults | CA92.1 | `Core/Settings.swift` and `Core/HostStore.swift`: settings and saved hosts in this app's defaults domain |
| System boot time | 35F9.1 | Rust monotonic timers and elapsed session/decode measurements; no device-boot identifier is exposed |

The approved reasons are documented by [Apple](https://developer.apple.com/documentation/bundleresources/describing-use-of-required-reason-api).
This is not a completed binary/dependency audit or an App Store privacy label.
Inspect the linked release binary for additional required-reason APIs, trace
their actual call paths, and declare only justified uses before submission.
Debug launch arguments and diagnostics must remain absent from the Release app.

## Reviewer access

Use a dedicated, nonprivate desktop in an isolated review tailnet, with the
reviewer device admitted through the normal installed-Tailscale identity path.
The current identity validator rejects externally shared peers (`SharedPeer`);
a machine-sharing invitation is therefore not sufficient reviewer access.
Do not broaden production tailnet grants or remove this refusal to enable review.

The reviewer needs the exact host name and port and instructions for the first
connection. Directory discovery needs an already saved, reachable host; it does
not discover an arbitrary tailnet from an empty installation. Verify the entire
flow from a clean iOS app state, including view-only and control modes, and
retain evidence that the desktop contains no owner data or credentials.

## Interrupted viewing

Backgrounding the app, losing connectivity, or changing the active interface
types closes the original session. The machine picker offers **Resume viewing**
when the device is online. An interrupted live session needs its original pin
and a successful close report before the app can reopen it. A failed or
unverified close returns the user to machine selection. A cancelled start
that never opened a service can be offered directly.

Resume uses the original destination and performs a new authenticated dial.
View-only sessions stay view-only. For control-capable sessions, the user
must request control again, including when Control on Connect is enabled.
Resume clears clipboard sharing and the one-use file-receive choice. Validate
Wi-Fi/cellular transitions and loss of the VPN on physical devices as part of
the network-transition gate. An interface notification alone cannot establish
which route a connection takes.

## Required evidence before submission

- iPhone and iPad XCTest results, plus actual remote-session interaction,
  rotation, background revocation, reconnect, and failure handling.
- Signed archive with the correct identifier/version, icon, privacy manifest,
  entitlements, and exact source/dependency provenance.
- Working host installation path and reviewer access; host prerequisites
  disclosed before purchase, with verified screenshots and support/privacy URLs.
- Build processed by App Store Connect and final submission validation passed.

A successful compilation alone does not satisfy these checks. Do not claim
physical-device or reviewer qualification from Simulator or namespace tests.
