# Install the Windows host

The Windows installer can install the existing interactive desktop host with
`-Host`. The default remains a client-only installation. Hosting runs as the
signed-in user in that user's interactive session, with a per-user sign-in
task. It does not install a Session 0 service, save a password, or request an
elevated task.

Until a release containing the host components is published, use the source
build:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/Dicklesworthstone/franken_remote/main/install.ps1))) -Host -FromSource
```

A host-enabled release uses the same command without `-FromSource`. Its verified
archive must contain `frd.exe`, `fr-media-worker.exe`, and `host.ps1` in addition
to the existing client binaries. An older client-only archive is refused before
an existing host is changed. The source build enables `frd/windows-host` and
`fr-native/windows-host-media`; it does not add an FFmpeg or software fallback.

`-Host` permits viewing after **local approval**, with **own-user** Tailscale
scope. `-HostControl` additionally offers control and the controller's text
clipboard after approval, and implies `-Host`. Files, audio, and microphone
forwarding are not enabled by this installer. The original host's native
session, input, approval, certificate, ingress, and hardware checks still apply.

```powershell
# Also allow a viewer to request control and clipboard sharing.
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/Dicklesworthstone/franken_remote/main/install.ps1))) -HostControl -FromSource

# Register for the next sign-in without starting the host or client now.
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/Dicklesworthstone/franken_remote/main/install.ps1))) -Host -NoLaunch
```

The one-liner environment equivalents are `FR_INSTALL_HOST=1` and
`FR_INSTALL_HOST_CONTROL=1`.

## Optional host playback audio

Windows host playback is an explicit source-build option. The default installer
keeps its existing `windows-host-media` worker and does not require an Opus SDK.
Build the audio worker with a trusted static libopus archive for the exact target
architecture and ABI. `FR_NATIVE_OPUS_LIBRARY_DIR` must be an absolute directory
containing `opus.lib` for MSVC or `libopus.a` for GNU. Record the archive's source,
license and build configuration with the package; the build downloads no codec
and introduces no runtime DLL search path.

```powershell
$env:FR_NATIVE_OPUS_LIBRARY_DIR = 'C:\fr-sdk\opus-x86_64-msvc\lib'
cargo build --release --locked -p frd --features windows-host --bin frd
cargo build --release --locked -p fr-native --no-default-features --features windows-host-audio --bin fr-media-worker
.\target\release\frd.exe run --encoder mediafoundation --approval local --sharing own-user --audio --audio-endpoint-wide
```

**`--audio-endpoint-wide` authorizes the complete default render endpoint's
playback mix, which can include audio from other Terminal Services sessions.**
It is not isolated to the selected display, application or desktop user.
`--audio` without this explicit acknowledgment is refused on Windows. The flag
requires `--audio`, is Windows-only, and never enables microphone forwarding.
Linux's `--audio-server` and `--audio-sink` selections are refused on Windows.
The receiving Linux or macOS client must independently request `--audio`.

Only an admitted viewer's audio request starts a separate supervised
`fr-media-worker --audio`. That worker pins the default endpoint at capture
start, requests shared-mode WASAPI loopback in 48 kHz signed-16 mono/stereo with
the engine's format conversion, and uses the existing bounded Opus downlink.
Unsupported initialization leaves audio unavailable. Device invalidation or a
default-endpoint change ends that source; it never moves a live stream to the
replacement. A worker built without `windows-host-audio` explicitly refuses the
audio role. Keep the matching `frd.exe` and worker together; the default host
installer does not modify its fixed sign-in arguments to enable this option.

Native packet timestamps retain their original source-age deadlines through
packing, encoding and final publication. Unknown timestamps and reversed clocks
are terminal, stale samples are discarded, and a quiet endpoint produces no
invented audio. Capture and codec work remain in the existing parent-supervised
process; this is process isolation, without an OS-sandbox claim.

Portable policy tests and Windows GNU compilation cover source timing, PCM
bounds, private records and explicit scope selection. These checks do not link
or execute a Windows worker. Real endpoint conversion, audibility, other-session
scope, device changes, suspend and worker cleanup still require Windows device
qualification before claiming support. The scope
and loopback contract are documented by Microsoft's
[loopback recording guide](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording)
and [stream flags](https://learn.microsoft.com/en-us/windows/win32/coreaudio/audclnt-streamflags-xxx-constants).

## Start, stop, and remove

Hosting has its own Start-menu commands and **FrankenRemote Host** entry in
Settings → Apps. Its program files live in
`%LOCALAPPDATA%\Programs\FrankenRemoteHost`, separately from the desktop client.

- **Start FrankenRemote hosting** enables its sign-in task and requests a start.
- **Stop FrankenRemote hosting** disables future sign-in starts and requests
  the original host's ordinary local revoke. Start enables it again.
- The host's existing **Stop sharing** button ends the current run through its
  normal teardown. The sign-in task can start it at the next sign-in.
- Removing **FrankenRemote Host** stops and removes hosting. Removing the
  separately listed **FrankenRemote** client keeps the host installed.

The installer also offers explicit host-only removal:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/Dicklesworthstone/franken_remote/main/install.ps1))) -Host -Uninstall
```

Without `-Host`, `-Uninstall` retains its existing client-only meaning. Host
removal preserves Tailscale, the desktop client, and saved host policy.

**The existing Windows Stop sharing window remains visible for the entire host
run, including idle listening.** Its current wording does not indicate whether
a viewer is connected. This installer does not replace it with a tray icon or
change the host's visible-before-listening rule. See
[the observation indicator](observation-indicator.md#windows-hosting).

Locking, switching away from, logging out of, or suspending the original session
still ends sharing. This task does not restart on failure or unlock and cannot
undo a local Stop by immediately starting another instance. If Tailscale is not
ready at sign-in, start hosting from the Start menu after it connects. Task
state and a visible indicator do not establish successful HEVC encoding or
end-to-end connectivity.

## Updates and ownership

The installer accepts only its original current-user task, exact executable
path, interactive principal, and fixed host arguments. It refuses modified or
foreign tasks, linked program paths, and program directories writable by other
untrusted principals. The task has one instance, ordinary interactive scheduler
priority, no execution-time kill, and no automatic retry.

An update first prepares both host executables and their matching helper in one
bounded staging directory. Before replacing files, it disables the old task,
locates the original executable and session, and sends a bounded `WM_CLOSE` to
the visible native `FrankenRemoteIndicator` window. The existing host handles
that message as local revoke. A local-approval dialog or hidden clipboard icon
cannot satisfy this match. The installer holds original process identities and
waits for the daemon and its installed capture worker to exit; it never falls
back to killing a process.

Task-registration, metadata, or observed-start failure restores the previous
files and task only after the new owner is positively retired. Uncertain cleanup
keeps sign-in startup disabled and retains the previous installation. A second
update is refused while a staging or previous directory remains unresolved.
Unknown files are preserved, never recursively swept away. This bounded failure
state requires attention from the signed-in user; the installer does not claim a
successful upgrade or silently restart old authority.

## Validation scope

`scripts/windows/test-host-installer.ps1` runs the actual task-definition policy
and filesystem transaction with injected task/registry/process operations. It
checks ownership refusals, XML escaping, explicit control, missing payloads,
update ordering, task/start/metadata rollback, uncertain retirement, preserved
unrelated content, host-only removal, and PowerShell's `-Host` alias. It also
compiles the narrow native window boundary. Run it with PowerShell 5.1+ or `pwsh`.

These tests are portable source and transaction evidence. They do not execute
Windows Task Scheduler or native window messages, and they do not qualify a
Windows host's hardware encoder, lock/revoke behavior, or a live client session.
Those remain the live acceptance work for `fr-installer-windows-host-6132`.

Task semantics follow Microsoft's [Task Scheduler schema](https://learn.microsoft.com/en-us/windows/win32/taskschd/task-scheduler-schema),
[interactive task principal](https://learn.microsoft.com/en-us/powershell/module/scheduledtasks/new-scheduledtaskprincipal),
and [task settings](https://learn.microsoft.com/en-us/powershell/module/scheduledtasks/new-scheduledtasksettingsset).
