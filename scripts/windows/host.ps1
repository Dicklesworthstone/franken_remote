# Per-user interactive Windows host installation and lifecycle.
# Shipped with the two host executables in the verified release archive.
# Windows PowerShell 5.1 compatible; no administrator account or stored password.
Param(
  [ValidateSet("Install", "Start", "Stop", "Status", "Uninstall")]
  [string]$Action = "Status",
  [string]$Source,
  [switch]$Control,
  [switch]$NoLaunch,
  [switch]$Quiet
)

$ErrorActionPreference = "Stop"
$script:HostMarker = "FrankenRemote interactive host v1"
$script:HostFiles = @("frd.exe", "fr-media-worker.exe", "host.ps1", "host.json")
$script:HostRegistryNames = @("DisplayName", "Publisher", "InstallLocation", "UninstallString", "QuietUninstallString", "NoModify", "NoRepair")

function Write-HostResult([string]$Message) { if (-not $Quiet) { Write-Host $Message } }

function Get-HostLayout {
  if ($PSVersionTable.PSVersion.Major -ge 6 -and -not $IsWindows) { throw "The host installer requires Windows." }
  if ($PSVersionTable.PSVersion -lt [Version]"5.1") { throw "PowerShell 5.1 or newer is required." }
  if (-not [Environment]::UserInteractive -or [Diagnostics.Process]::GetCurrentProcess().SessionId -eq 0) {
    throw "Run this installer in the signed-in user's interactive Windows session, not a service or remote automation session."
  }
  $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
  if ($sid -notmatch '^S-1-5-21-(\d+-){3}\d+$' -and $sid -notmatch '^S-1-12-1-(\d+-){3}\d+$') {
    throw "The current account is not an ordinary interactive user."
  }
  $local = [Environment]::GetFolderPath("LocalApplicationData")
  $menu = [Environment]::GetFolderPath("Programs")
  if (-not $local -or -not $menu) { throw "The user's program folders are unavailable." }
  $root = Join-Path $local "Programs\FrankenRemoteHost"
  return @{
    Sid = $sid; Session = [Diagnostics.Process]::GetCurrentProcess().SessionId
    Root = $root; Current = (Join-Path $root "current")
    Staging = (Join-Path $root "staging"); Previous = (Join-Path $root "previous")
    Task = "FrankenRemote-Host-$sid"; Local = $local
    Registry = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\FrankenRemoteHost"
    Links = @((Join-Path $menu "Start FrankenRemote hosting.lnk"), (Join-Path $menu "Stop FrankenRemote hosting.lnk"))
    PowerShell = (Join-Path $env:SystemRoot "System32\WindowsPowerShell\v1.0\powershell.exe")
  }
}

function Test-HostPathEqual([string]$Left, [string]$Right) {
  return [string]::Equals($Left.TrimEnd('\'), $Right.TrimEnd('\'), [StringComparison]::OrdinalIgnoreCase)
}

# No junction/symlink may redirect any part of an executable or helper path.
function Assert-HostPlainPath([string]$Path) {
  $cursor = [IO.Path]::GetFullPath($Path)
  while ($cursor) {
    if (Test-Path -LiteralPath $cursor) {
      $item = Get-Item -LiteralPath $cursor -Force
      if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "A host installation path is a link: $cursor" }
    }
    $parent = [IO.Path]::GetDirectoryName($cursor)
    if ($parent -eq $cursor) { break }
    $cursor = $parent
  }
}

function Assert-HostPrivateDirectory([string]$Path, [string]$Sid) {
  Assert-HostPlainPath $Path
  if (-not (Test-Path -LiteralPath $Path)) { return }
  $acl = Get-Acl -LiteralPath $Path
  $trusted = @($Sid, "S-1-5-18", "S-1-5-32-544") # this user, SYSTEM, Administrators
  $owner = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value
  if ($trusted -notcontains $owner) { throw "The host directory has an unexpected owner: $Path" }
  $write = [Security.AccessControl.FileSystemRights]"Write,Delete,DeleteSubdirectoriesAndFiles,ChangePermissions,TakeOwnership"
  foreach ($ace in $acl.Access) {
    if ($ace.AccessControlType -ne "Allow" -or ($ace.PropagationFlags -band [Security.AccessControl.PropagationFlags]::InheritOnly)) { continue }
    $identity = $ace.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value
    if (($ace.FileSystemRights -band $write) -and $trusted -notcontains $identity) {
      throw "The host directory is writable by another principal: $Path"
    }
  }
}

function Assert-HostLayout($Layout) {
  foreach ($path in @($Layout.Local, (Split-Path -Parent $Layout.Root), $Layout.Root, $Layout.Current)) {
    Assert-HostPrivateDirectory $path $Layout.Sid
  }
  foreach ($name in $script:HostFiles) { Assert-HostPrivateDirectory (Join-Path $Layout.Current $name) $Layout.Sid }
  foreach ($link in $Layout.Links) { Assert-HostPlainPath $link }
  Assert-HostPlainPath $Layout.PowerShell
}

function Get-HostArguments([bool]$WithControl) {
  $arguments = "run --encoder mediafoundation --approval local --sharing own-user --directory --adaptive-capture --json"
  if ($WithControl) { $arguments += " --control" }
  return $arguments
}

function ConvertTo-HostTaskXml($Layout, [bool]$WithControl) {
  $escape = { param($value) [Security.SecurityElement]::Escape([string]$value) }
  $sid = & $escape $Layout.Sid
  $exe = & $escape (Join-Path $Layout.Current "frd.exe")
  $directory = & $escape $Layout.Current
  $arguments = & $escape (Get-HostArguments $WithControl)
  return @"
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>$($script:HostMarker)</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>$sid</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="HostUser"><UserId>$sid</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><AllowHardTerminate>false</AllowHardTerminate><StartWhenAvailable>false</StartWhenAvailable><RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable><Enabled>true</Enabled><Hidden>false</Hidden><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><Priority>4</Priority></Settings>
  <Actions Context="HostUser"><Exec><Command>$exe</Command><Arguments>$arguments</Arguments><WorkingDirectory>$directory</WorkingDirectory></Exec></Actions>
</Task>
"@
}

# Refuse a name collision or locally modified task, instead of overwriting it.
function Assert-OwnedHostTask($Layout, [string]$Text) {
  [xml]$xml = $Text
  $task = $xml.Task
  if ($task.NamespaceURI -ne "http://schemas.microsoft.com/windows/2004/02/mit/task" -or
      $task.RegistrationInfo.Description -cne $script:HostMarker -or
      @($task.Principals.Principal).Count -ne 1 -or
      $task.Principals.Principal.UserId -cne $Layout.Sid -or
      $task.Principals.Principal.LogonType -cne "InteractiveToken" -or
      $task.Principals.Principal.RunLevel -cne "LeastPrivilege" -or
      $task.Principals.Principal.id -cne "HostUser" -or $task.Actions.Context -cne "HostUser" -or
      @($task.Triggers.ChildNodes).Count -ne 1 -or $task.Triggers.LogonTrigger.UserId -cne $Layout.Sid -or
      $task.Triggers.LogonTrigger.Enabled -cne "true" -or
      $task.Settings.MultipleInstancesPolicy -cne "IgnoreNew" -or
      $task.Settings.AllowHardTerminate -cne "false" -or
      $task.Settings.StartWhenAvailable -cne "false" -or
      $task.Settings.ExecutionTimeLimit -cne "PT0S" -or $task.Settings.Priority -cne "4" -or
      $null -ne $task.Settings.RestartOnFailure -or
      @($task.Actions.ChildNodes).Count -ne 1 -or
      -not (Test-HostPathEqual $task.Actions.Exec.Command (Join-Path $Layout.Current "frd.exe")) -or
      -not (Test-HostPathEqual $task.Actions.Exec.WorkingDirectory $Layout.Current) -or
      @((Get-HostArguments $false), (Get-HostArguments $true)) -cnotcontains [string]$task.Actions.Exec.Arguments) {
    throw "The scheduled task is not this user's unchanged FrankenRemote host task; it was not modified."
  }
}

function Get-OwnedHostTask($Layout) {
  # Enumerate first: querying an empty root/name can itself be a CIM not-found
  # error. An enumeration failure still refuses; it is never treated as absent.
  $tasks = @(Get-ScheduledTask -ErrorAction Stop | Where-Object { $_.TaskPath -ceq '\' -and $_.TaskName -ceq $Layout.Task })
  if ($tasks.Count -eq 0) { return $null }
  if ($tasks.Count -ne 1) { throw "The host task identity is ambiguous." }
  $xml = Export-ScheduledTask -TaskPath '\' -TaskName $Layout.Task -ErrorAction Stop
  Assert-OwnedHostTask $Layout $xml
  return @{ Task = $tasks[0]; Xml = [string]$xml }
}

function Initialize-HostWindowApi {
  if ("FrankenRemote.Installer.HostWindow" -as [type]) { return }
  Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
namespace FrankenRemote.Installer {
  public static class HostWindow {
    private delegate bool Visitor(IntPtr window, IntPtr data);
    [DllImport("user32.dll", SetLastError=true)] private static extern bool EnumWindows(Visitor visitor, IntPtr data);
    [DllImport("user32.dll")] private static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] private static extern int GetClassName(IntPtr window, StringBuilder name, int maximum);
    [DllImport("user32.dll", EntryPoint="SendMessageTimeoutW", SetLastError=true)] private static extern IntPtr SendMessageTimeout(IntPtr window, uint message, IntPtr wparam, IntPtr lparam, uint flags, uint timeout, out UIntPtr result);
    public static bool RequestClose(int process, bool close, uint timeout) {
      var matches = new List<IntPtr>();
      Visitor visitor = delegate(IntPtr window, IntPtr data) {
        uint owner;
        GetWindowThreadProcessId(window, out owner);
        if (owner != (uint)process || !IsWindowVisible(window)) return true;
        var name = new StringBuilder(128);
        if (GetClassName(window, name, name.Capacity) > 0 && name.ToString() == "FrankenRemoteIndicator") matches.Add(window);
        return true;
      };
      if (!EnumWindows(visitor, IntPtr.Zero)) throw new InvalidOperationException("Cannot enumerate the original host indicator.");
      if (matches.Count > 1) throw new InvalidOperationException("The original host indicator is ambiguous.");
      if (matches.Count == 0) return false;
      uint current;
      GetWindowThreadProcessId(matches[0], out current);
      if (current != (uint)process || !IsWindowVisible(matches[0])) return false;
      if (!close) return true;
      UIntPtr result;
      // WM_CLOSE enters fr_winsys::indicator's ordinary local-revoke path.
      // SMTO_BLOCK | SMTO_ABORTIFHUNG; no broadcast, kill, or injected click.
      if (timeout == 0 || SendMessageTimeout(matches[0], 0x0010, IntPtr.Zero, IntPtr.Zero, 3, Math.Min(500U, timeout), out result) == IntPtr.Zero)
        throw new InvalidOperationException("The original host indicator did not answer the stop request.");
      return true;
    }
  }
}
'@
}

function Request-HostIndicator($Process, [bool]$Close, [uint32]$Timeout = 500) {
  return [FrankenRemote.Installer.HostWindow]::RequestClose($Process.Id, $Close, $Timeout)
}

# Hold process handles while acting/waiting: a reused PID is never a replacement
# owner. Full paths, current session and creation time identify each original.
function Get-OwnedHostProcesses($Layout) {
  $result = @()
  foreach ($process in @(Get-Process -Name frd, fr-media-worker -ErrorAction SilentlyContinue)) {
    try {
      if ($process.HasExited) { $process.Dispose(); continue }
      $path = $process.Path
      $expected = Join-Path $Layout.Current ($process.ProcessName + ".exe")
      if (-not $path -or -not (Test-HostPathEqual $path $expected)) { $process.Dispose(); continue }
      $null = $process.Handle
      if ($process.SessionId -ne $Layout.Session) { throw "The installed host is running in a different Windows session; stop it there before changing this installation." }
      $null = $process.StartTime
      $result += $process
    } catch {
      $process.Dispose()
      foreach ($opened in $result) { $opened.Dispose() }
      throw
    }
  }
  return $result
}

function Stop-OwnedHost($Layout) {
  $owned = Get-OwnedHostTask $Layout
  if ($owned) { Disable-ScheduledTask -TaskPath '\' -TaskName $Layout.Task -ErrorAction Stop | Out-Null }
  Initialize-HostWindowApi
  $deadline = [Diagnostics.Stopwatch]::StartNew()
  $asked = @{}
  $watched = @{}
  try {
    while ($deadline.Elapsed.TotalSeconds -lt 15) {
      $processes = @(Get-OwnedHostProcesses $Layout)
      foreach ($process in $processes) {
        $key = "$($process.Id):$($process.StartTime.ToUniversalTime().Ticks)"
        if ($watched.ContainsKey($key)) { $process.Dispose() } else {
          if ($watched.Count -ge 16) { $process.Dispose(); throw "Too many original host processes appeared during bounded retirement; startup remains disabled." }
          $watched[$key] = $process
        }
      }
      $alive = 0
      foreach ($process in $watched.Values) {
        if ($process.HasExited) { continue }
        $alive++
        if ($process.ProcessName -ine "frd") { continue }
        $key = "$($process.Id):$($process.StartTime.ToUniversalTime().Ticks)"
        $remaining = 15000 - $deadline.ElapsedMilliseconds
        if ($remaining -le 0) { break }
        if (-not $asked.ContainsKey($key) -and (Request-HostIndicator $process $true ([uint32][Math]::Min(500, $remaining)))) { $asked[$key] = $true }
      }
      $owned = Get-OwnedHostTask $Layout
      if ($alive -eq 0 -and $processes.Count -eq 0 -and (-not $owned -or $owned.Task.State -notin @("Running", "Queued"))) { return }
      Start-Sleep -Milliseconds 100
    }
  } finally { foreach ($process in $watched.Values) { $process.Dispose() } }
  throw "Host cleanup was not confirmed within 15 seconds. Sign-in startup remains disabled; use the original Stop sharing window and retry. No process was killed or replaced."
}

function Start-OwnedHost($Layout) {
  $owned = Get-OwnedHostTask $Layout
  if (-not $owned) { throw "FrankenRemote hosting is not installed." }
  $existing = @(Get-OwnedHostProcesses $Layout)
  try {
    if ($existing.Count -gt 0 -and $owned.Task.State -notin @("Running", "Queued")) {
      throw "An installed host process exists outside the running task. Stop that original owner before starting the task."
    }
  } finally { foreach ($process in $existing) { $process.Dispose() } }
  Enable-ScheduledTask -TaskPath '\' -TaskName $Layout.Task -ErrorAction Stop | Out-Null
  Start-ScheduledTask -TaskPath '\' -TaskName $Layout.Task -ErrorAction Stop
  Initialize-HostWindowApi
  $deadline = [Diagnostics.Stopwatch]::StartNew()
  while ($deadline.Elapsed.TotalSeconds -lt 15) {
    $processes = @(Get-OwnedHostProcesses $Layout)
    try {
      $hosts = @($processes | Where-Object { $_.ProcessName -ieq "frd" -and -not $_.HasExited })
      if ($hosts.Count -gt 1) { throw "More than one original host process is present; the task start cannot be confirmed." }
      $owned = Get-OwnedHostTask $Layout
      if ($owned -and $owned.Task.State -eq "Running" -and $hosts.Count -eq 1 -and (Request-HostIndicator $hosts[0] $false)) {
        # A manual process/old visible window is not evidence that this task ran.
        $owned = Get-OwnedHostTask $Layout
        if ($owned -and $owned.Task.State -eq "Running" -and -not $hosts[0].HasExited) { return }
      }
    } finally { foreach ($process in $processes) { $process.Dispose() } }
    Start-Sleep -Milliseconds 100
  }
  throw "The host's visible Stop sharing window did not appear within 15 seconds. Hardware capture and Tailscale readiness have not been established."
}

function Assert-OwnedHostDirectory([string]$Path) {
  if (-not (Test-Path -LiteralPath $Path)) { return }
  Assert-HostPlainPath $Path
  $items = @(Get-ChildItem -LiteralPath $Path -Force)
  foreach ($item in $items) {
    if ($item.PSIsContainer -or $script:HostFiles -cnotcontains $item.Name -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
      throw "Unexpected content in the host program directory was preserved: $Path"
    }
  }
}

function Remove-OwnedHostDirectory([string]$Path) {
  if (-not (Test-Path -LiteralPath $Path)) { return }
  Assert-OwnedHostDirectory $Path
  $items = @(Get-ChildItem -LiteralPath $Path -Force)
  foreach ($item in $items) { Remove-Item -LiteralPath $item.FullName -Force }
  Remove-Item -LiteralPath $Path
}

function Get-HostRegistration($Layout) {
  $registry = @{}
  $exists = Test-Path -LiteralPath $Layout.Registry
  if ($exists) {
    $values = Get-ItemProperty -LiteralPath $Layout.Registry
    foreach ($name in $script:HostRegistryNames) {
      $property = $values.PSObject.Properties[$name]
      if ($property) { $registry[$name] = $property.Value }
    }
  }
  $links = @{}
  foreach ($path in $Layout.Links) {
    if (Test-Path -LiteralPath $path) {
      if ((Get-Item -LiteralPath $path).Length -gt 65536) { throw "An existing host shortcut is not a bounded shell link." }
      $links[$path] = [IO.File]::ReadAllBytes($path)
    } else { $links[$path] = $null }
  }
  return @{ Registry = $registry; RegistryExists = $exists; Links = $links }
}

function Set-HostRegistration($Layout) {
  $helper = Join-Path $Layout.Current "host.ps1"
  $prefix = "-NoProfile -ExecutionPolicy Bypass -File `"$helper`""
  $command = "`"$($Layout.PowerShell)`" $prefix -Action Uninstall"
  New-Item -Path $Layout.Registry -Force | Out-Null
  foreach ($pair in @(
    @("DisplayName", "FrankenRemote Host"), @("Publisher", "FrankenRemote"),
    @("InstallLocation", $Layout.Root), @("UninstallString", $command), @("QuietUninstallString", "$command -Quiet")
  )) { New-ItemProperty -LiteralPath $Layout.Registry -Name $pair[0] -Value $pair[1] -PropertyType String -Force | Out-Null }
  foreach ($name in @("NoModify", "NoRepair")) { New-ItemProperty -LiteralPath $Layout.Registry -Name $name -Value 1 -PropertyType DWord -Force | Out-Null }
  $shell = New-Object -ComObject WScript.Shell
  for ($i = 0; $i -lt $Layout.Links.Count; $i++) {
    $operation = @("Start", "Stop")[$i]
    $link = $shell.CreateShortcut($Layout.Links[$i])
    $link.TargetPath = $Layout.PowerShell
    $link.Arguments = "$prefix -Action $operation"
    $link.WorkingDirectory = $Layout.Current
    $link.Description = "$operation FrankenRemote hosting for this signed-in user"
    $link.Save()
  }
}

function Restore-HostRegistration($Layout, $Saved) {
  # Only our fixed values are owned. Preserve other values and subkeys even
  # when restoring an interrupted first install or removing the host.
  if ($Saved.RegistryExists -or $Saved.Registry.Count) {
    New-Item -Path $Layout.Registry -Force | Out-Null
  }
  if (Test-Path -LiteralPath $Layout.Registry) {
    $present = Get-ItemProperty -LiteralPath $Layout.Registry
    foreach ($name in $script:HostRegistryNames) {
      if (-not $Saved.Registry.ContainsKey($name)) {
        if ($null -ne $present.PSObject.Properties[$name]) { Remove-ItemProperty -LiteralPath $Layout.Registry -Name $name }
        continue
      }
      $type = "String"; if ($name -in @("NoModify", "NoRepair")) { $type = "DWord" }
      New-ItemProperty -LiteralPath $Layout.Registry -Name $name -Value $Saved.Registry[$name] -PropertyType $type -Force | Out-Null
    }
    $key = Get-Item -LiteralPath $Layout.Registry
    if (-not $Saved.RegistryExists -and $key.GetValueNames().Count -eq 0 -and $key.SubKeyCount -eq 0) { Remove-Item -LiteralPath $Layout.Registry }
  }
  foreach ($path in $Layout.Links) {
    if ($null -eq $Saved.Links[$path]) {
      if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path }
    } else { [IO.File]::WriteAllBytes($path, $Saved.Links[$path]) }
  }
}

function Install-OwnedHost($Layout, [string]$Payload, [bool]$WithControl, [bool]$Launch) {
  foreach ($name in @("frd.exe", "fr-media-worker.exe", "host.ps1")) {
    $path = Join-Path $Payload $name
    Assert-HostPlainPath $path
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "The payload has no $name. Choose a host-enabled release or use install.ps1 -Host -FromSource." }
  }
  $original = Get-OwnedHostTask $Layout
  $saved = Get-HostRegistration $Layout
  Assert-OwnedHostDirectory $Layout.Current
  if (-not $original -and ($saved.Registry.Count -or @($saved.Links.Values | Where-Object { $null -ne $_ }).Count)) {
    throw "Existing host registration or shortcuts have no matching owned task; they were preserved."
  }
  if ((Test-Path -LiteralPath $Layout.Previous) -or (Test-Path -LiteralPath $Layout.Staging)) {
    throw "An unfinished host update is retained in $($Layout.Root). Resolve it before starting another update; no files were replaced."
  }
  if ((Test-Path -LiteralPath $Layout.Current) -and -not $original) {
    throw "An existing host program directory has no matching owned task; it was preserved."
  }
  $published = $false
  $retired = $false
  $metadataTouched = $false
  try {
    New-Item -ItemType Directory -Path $Layout.Staging -Force | Out-Null
    foreach ($name in @("frd.exe", "fr-media-worker.exe", "host.ps1")) { Copy-Item -LiteralPath (Join-Path $Payload $name) -Destination (Join-Path $Layout.Staging $name) }
    @{ schema = 1; owner = $Layout.Sid; control = $WithControl } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $Layout.Staging "host.json") -Encoding UTF8
    Stop-OwnedHost $Layout
    $retired = $true
    if (Test-Path -LiteralPath $Layout.Current) { Move-Item -LiteralPath $Layout.Current -Destination $Layout.Previous }
    Move-Item -LiteralPath $Layout.Staging -Destination $Layout.Current
    $published = $true
    Register-ScheduledTask -TaskPath '\' -TaskName $Layout.Task -Xml (ConvertTo-HostTaskXml $Layout $WithControl) -Force -ErrorAction Stop | Out-Null
    $null = Get-OwnedHostTask $Layout
    $metadataTouched = $true
    Set-HostRegistration $Layout
    if ($Launch) { Start-OwnedHost $Layout }
  } catch {
    $failure = $_
    if ($published) {
      # A failed start may have created a real process. Retire it positively
      # before restoring any old executable or task. Uncertainty stays disabled.
      try { Stop-OwnedHost $Layout } catch { throw "Host update failed and cleanup is unconfirmed. Startup remains disabled; the previous installation is retained at $($Layout.Previous). $($_.Exception.Message)" }
      $current = Get-OwnedHostTask $Layout
      if ($current) { Unregister-ScheduledTask -TaskPath '\' -TaskName $Layout.Task -Confirm:$false -ErrorAction Stop }
      Remove-OwnedHostDirectory $Layout.Current
    }
    if ($retired -and (Test-Path -LiteralPath $Layout.Previous)) { Move-Item -LiteralPath $Layout.Previous -Destination $Layout.Current }
    if ($retired -and $original) { Register-ScheduledTask -TaskPath '\' -TaskName $Layout.Task -Xml $original.Xml -Force -ErrorAction Stop | Out-Null }
    if ($metadataTouched) { Restore-HostRegistration $Layout $saved }
    Remove-OwnedHostDirectory $Layout.Staging
    throw $failure
  }
  Remove-OwnedHostDirectory $Layout.Previous
  Write-HostResult "FrankenRemote Host is installed for this user's sign-in. Its visible Stop sharing window remains required, including while idle."
  Write-HostResult "Local approval is required; only this user's tailnet machines are eligible. Use Start/Stop FrankenRemote hosting in the Start menu."
  if ($Launch) { Write-HostResult "The original host indicator is visible. Tailscale connectivity and hardware HEVC capture still have their normal startup checks." }
  else { Write-HostResult "Hosting was not started; it starts at the next sign-in or from Start FrankenRemote hosting." }
}

function Uninstall-OwnedHost($Layout) {
  $owned = Get-OwnedHostTask $Layout
  if (-not $owned) { throw "No owned FrankenRemote host task is installed; unrelated files and tasks were preserved." }
  if ((Test-Path -LiteralPath $Layout.Previous) -or (Test-Path -LiteralPath $Layout.Staging)) { throw "An unfinished host update is retained; resolve it before uninstalling." }
  Assert-OwnedHostDirectory $Layout.Current
  Stop-OwnedHost $Layout
  Unregister-ScheduledTask -TaskPath '\' -TaskName $Layout.Task -Confirm:$false -ErrorAction Stop
  Restore-HostRegistration $Layout @{ Registry = @{}; Links = @{} }
  Remove-OwnedHostDirectory $Layout.Current
  if (@(Get-ChildItem -LiteralPath $Layout.Root -Force).Count -eq 0) { Remove-Item -LiteralPath $Layout.Root }
  Write-HostResult "FrankenRemote hosting was stopped and removed. The desktop client, Tailscale and saved host policy were retained."
}

function New-HostInstallMutex([string]$Name) {
  return New-Object Threading.Mutex($false, $Name)
}

function Invoke-WithHostInstallMutex([string]$Sid, [scriptblock]$Body) {
  # This user's files and task are shared across logon sessions. A Local
  # namespace mutex would allow two sessions to replace the same installation.
  # Creating a Global mutex (unlike a Global file mapping) needs no elevation.
  $mutex = New-HostInstallMutex "Global\FrankenRemoteHostInstall-$Sid"
  $owned = $false
  try {
    try { $owned = $mutex.WaitOne(0) }
    catch [Threading.AbandonedMutexException] { $owned = $true }
    if (-not $owned) { throw "Another host installer or lifecycle operation is running." }
    # An abandoned mutex transfers ownership; it does not bless the previous
    # operation. The ordinary retained-state checks still run inside this lock.
    & $Body
  } finally {
    try { if ($owned) { $mutex.ReleaseMutex() } }
    finally { $mutex.Dispose() }
  }
}

function Invoke-HostMain {
  $layout = Get-HostLayout
  Assert-HostLayout $layout
  Invoke-WithHostInstallMutex $layout.Sid {
    if ($Action -notin @("Stop", "Status") -and ((Test-Path -LiteralPath $layout.Previous) -or (Test-Path -LiteralPath $layout.Staging))) {
      throw "An unfinished host update is retained. Startup remains disabled; stop and resolve the original owner before replacing or starting hosting."
    }
    switch ($Action) {
      "Install" {
        if (-not $Source) { throw "-Source must name the verified host payload directory." }
        Install-OwnedHost $layout $Source ([bool]$Control) (-not $NoLaunch)
      }
      "Start" {
        try { Start-OwnedHost $layout } catch { $failure = $_; Stop-OwnedHost $layout; throw $failure }
        Write-HostResult "FrankenRemote hosting is enabled and its original Stop sharing indicator is visible."
      }
      "Stop" { Stop-OwnedHost $layout; Write-HostResult "FrankenRemote hosting is stopped and sign-in startup is disabled. Start FrankenRemote hosting enables it again." }
      "Uninstall" { Uninstall-OwnedHost $layout }
      "Status" {
        $owned = Get-OwnedHostTask $layout
        if ($owned) { Write-Host "FrankenRemote Host task: $($owned.Task.State). This is task state, not a hardware or connection qualification." }
        else { Write-Host "FrankenRemote hosting is not installed." }
      }
    }
  }
}

# Tests load real policy/transaction functions without touching the local OS.
if ($env:FR_HOST_INSTALL_LIB_ONLY -eq "1") { return }
try { Invoke-HostMain } catch { Write-Error $_.Exception.Message -ErrorAction Continue; exit 1 }
