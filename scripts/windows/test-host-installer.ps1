# Portable execution of the real installer policy and filesystem transaction.
# Windows task/registry/process operations are injected below; this does not
# claim native Task Scheduler, window-message, HEVC or tailnet qualification.
$ErrorActionPreference = "Stop"
$env:FR_HOST_INSTALL_LIB_ONLY = "1"
. (Join-Path $PSScriptRoot "host.ps1")
$script:passed = 0
function Assert([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Rejects([scriptblock]$Body, [string]$Reason) {
  $refused = $false
  try { & $Body } catch { $refused = $true }
  Assert $refused $Reason
}
function Test([string]$Name, [scriptblock]$Body) {
  & $Body
  $script:passed++
  Write-Host "PASS $Name"
}

$script:scratch = Join-Path ([IO.Path]::GetTempPath()) ("fr-host-installer-tests-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $script:scratch | Out-Null
function Scenario {
  $root = Join-Path $script:scratch ([Guid]::NewGuid().ToString("N") + " alice & bob")
  $payload = Join-Path $root "payload"
  New-Item -ItemType Directory -Path $payload -Force | Out-Null
  foreach ($name in @("frd.exe", "fr-media-worker.exe", "host.ps1")) { Set-Content -LiteralPath (Join-Path $payload $name) -Value "new-$name" }
  $installed = Join-Path $root "host"
  $script:layout = @{
    Sid = "S-1-5-21-111-222-333-1001"; Session = 1; Root = $installed
    Current = (Join-Path $installed "current"); Staging = (Join-Path $installed "staging"); Previous = (Join-Path $installed "previous")
    Task = "FrankenRemote-Host-S-1-5-21-111-222-333-1001"
    Links = @((Join-Path $root "start.lnk"), (Join-Path $root "stop.lnk")); Registry = "test-only"
    PowerShell = (Join-Path $root "powershell.exe")
  }
  $script:payload = $payload
  $script:taskXml = $null
  $script:taskState = "Ready"
  $script:registration = @{ Registry = @{}; Links = @{} }
  $script:events = [Collections.Generic.List[string]]::new()
  $script:stopCount = 0
  $script:failStopAt = 0
  $script:failRegister = $false
  $script:failStart = $false
  $script:failMetadata = $false
}
function Existing {
  New-Item -ItemType Directory -Path $script:layout.Current -Force | Out-Null
  foreach ($name in $script:HostFiles) { Set-Content -LiteralPath (Join-Path $script:layout.Current $name) -Value "old-$name" }
  $script:taskXml = ConvertTo-HostTaskXml $script:layout $false
  $script:registration = @{ Registry = @{ DisplayName = "FrankenRemote Host" }; Links = @{} }
}

Test "task uses original interactive user, bounded ownership and local consent" {
  Scenario
  $text = ConvertTo-HostTaskXml $script:layout $false
  Assert-OwnedHostTask $script:layout $text
  [xml]$xml = $text
  Assert ($xml.Task.Principals.Principal.UserId -eq $script:layout.Sid) "wrong principal"
  Assert ($xml.Task.Principals.Principal.LogonType -eq "InteractiveToken") "non-interactive task"
  Assert ($xml.Task.Settings.MultipleInstancesPolicy -eq "IgnoreNew") "overlapping instances"
  Assert ($xml.Task.Settings.ExecutionTimeLimit -eq "PT0S") "scheduler would kill a long session"
  Assert ($xml.Task.Settings.Priority -eq "4") "scheduler would use below-normal default priority"
  Assert ($text -notmatch 'RestartOnFailure|Password|ServiceAccount|HighestAvailable') "restart/elevation credentials"
  Assert ($xml.Task.Actions.Exec.Arguments -match '--approval local --sharing own-user') "consent or scope weakened"
  Assert ($xml.Task.Actions.Exec.Arguments -notmatch '--control|--clipboard|--audio|--microphone') "default grants optional capability"
  Assert ($xml.Task.Actions.Exec.Command -eq (Join-Path $script:layout.Current "frd.exe")) "escaped path changed"
  Assert ($text.Contains('&amp;')) "XML path was not escaped"
  [xml]$controlled = ConvertTo-HostTaskXml $script:layout $true
  Assert ($controlled.Task.Actions.Exec.Arguments.EndsWith(" --control")) "explicit control lost"
}

Test "foreign and modified task definitions refuse before mutation" {
  Scenario
  $valid = ConvertTo-HostTaskXml $script:layout $false
  $edits = @(
    @($script:layout.Sid, "S-1-5-21-111-222-333-1002"), @("InteractiveToken", "S4U"),
    @("LeastPrivilege", "HighestAvailable"), @("frd.exe", "other.exe"),
    @("--approval local", "--approval none"), @("IgnoreNew", "Parallel"),
    @("<AllowHardTerminate>false", "<AllowHardTerminate>true"), @("<Priority>4", "<Priority>7"),
    @("<ExecutionTimeLimit>PT0S", "<ExecutionTimeLimit>PT72H"),
    @("<Settings>", "<Settings><RestartOnFailure><Interval>PT1M</Interval><Count>9</Count></RestartOnFailure>"),
    @("</Actions>", "<Exec><Command>other.exe</Command></Exec></Actions>")
  )
  foreach ($edit in $edits) {
    $changed = $valid.Replace($edit[0], $edit[1])
    Rejects { Assert-OwnedHostTask $script:layout $changed } "accepted modified task: $($edit[0])"
  }
}

# Exercise the real C# boundary's compilation, without invoking user32 on a
# non-Windows test host. Native indicator matching still requires Windows.
Test "narrow native window boundary compiles" { Initialize-HostWindowApi }

Test "cross-session mutex owns abandoned state and releases through refusal" {
  Add-Type -TypeDefinition @'
namespace FrankenRemote.Installer.Tests {
  public sealed class MutexProbe : System.IDisposable {
    public bool Abandoned;
    public int Released;
    public int Disposed;
    public bool WaitOne(int timeout) {
      if (Abandoned) throw new System.Threading.AbandonedMutexException();
      return true;
    }
    public void ReleaseMutex() { Released++; }
    public void Dispose() { Disposed++; }
  }
}
'@
  $script:mutexNames = [Collections.Generic.List[string]]::new()
  $script:mutexProbes = [Collections.Generic.List[object]]::new()
  $script:mutexBodies = 0
  $script:abandonMutex = $true
  function New-HostInstallMutex {
    Param($Name)
    $script:mutexNames.Add($Name)
    $mutex = [FrankenRemote.Installer.Tests.MutexProbe]::new()
    $mutex.Abandoned = $script:abandonMutex
    $script:mutexProbes.Add($mutex)
    return $mutex
  }
  $sid = "S-1-5-21-111-222-333-1001"
  Rejects { Invoke-WithHostInstallMutex $sid { $script:mutexBodies++; throw "retained state still refuses" } } "abandoned state bypassed refusal"
  $script:abandonMutex = $false
  Invoke-WithHostInstallMutex $sid { $script:mutexBodies++ }
  Assert ($script:mutexBodies -eq 2) "abandoned mutex did not transfer ownership to guarded checks"
  foreach ($probe in $script:mutexProbes) { Assert ($probe.Released -eq 1 -and $probe.Disposed -eq 1) "ownership leaked after success or failure" }
  Assert ($script:mutexNames.Count -eq 2 -and $script:mutexNames[0] -ceq "Global\FrankenRemoteHostInstall-$sid" -and $script:mutexNames[0] -ceq $script:mutexNames[1]) "same user did not use one cross-session mutex"
}

$script:realStart = ${function:Start-OwnedHost}
$script:realRestoreRegistration = ${function:Restore-HostRegistration}

Test "registry rollback preserves unknown values and subkeys" {
  # Run the real restoration function with only registry-provider operations
  # substituted; no filesystem or installed registry is touched.
  $script:registryValues = @{ DisplayName = "partly changed"; InstallLocation = "new"; OtherProductValue = "keep" }
  function Test-Path { Param($LiteralPath) return $true }
  function New-Item { Param($Path, [switch]$Force) }
  function Get-ItemProperty { Param($LiteralPath) return [pscustomobject]$script:registryValues }
  function Remove-ItemProperty { Param($LiteralPath, $Name) $script:registryValues.Remove($Name) }
  function New-ItemProperty { Param($LiteralPath, $Name, $Value, $PropertyType, [switch]$Force) $script:registryValues[$Name] = $Value }
  function Get-Item {
    Param($LiteralPath)
    $key = [pscustomobject]@{ SubKeyCount = 1 }
    $key | Add-Member -MemberType ScriptMethod -Name GetValueNames -Value { return @($script:registryValues.Keys) }
    return $key
  }
  function Remove-Item { throw "must not delete a key with unrelated data" }
  $layout = @{ Registry = "registry-fixture"; Links = @() }
  & $script:realRestoreRegistration $layout @{ Registry = @{ DisplayName = "original" }; RegistryExists = $true; Links = @{} }
  Assert ($script:registryValues.DisplayName -eq "original") "original value not restored"
  Assert (-not $script:registryValues.ContainsKey("InstallLocation")) "new owned value retained"
  Assert ($script:registryValues.OtherProductValue -eq "keep") "unrelated value lost"
  & $script:realRestoreRegistration $layout @{ Registry = @{}; RegistryExists = $false; Links = @{} }
  Assert ($script:registryValues.Count -eq 1 -and $script:registryValues.OtherProductValue -eq "keep") "uninstall removed unrelated registry content"
}

Test "manual process is not a positive task-start witness" {
  Scenario
  $process = [pscustomobject]@{ ProcessName = "frd"; Id = 42; HasExited = $false }
  $process | Add-Member -MemberType ScriptMethod -Name Dispose -Value { }
  function Get-OwnedHostTask { Param($Layout) return @{ Task = @{ State = "Ready" } } }
  function Get-OwnedHostProcesses { Param($Layout) return $process }
  function Enable-ScheduledTask { throw "must refuse before enabling a task" }
  Rejects { & $script:realStart $script:layout } "accepted manually launched process as task startup"
}

Test "visible process cannot qualify a task that is not Running" {
  Scenario
  $script:processQueries = 0
  $script:indicatorChecks = 0
  $process = [pscustomobject]@{ ProcessName = "frd"; Id = 42; HasExited = $false }
  $process | Add-Member -MemberType ScriptMethod -Name Dispose -Value { }
  function Get-OwnedHostTask { Param($Layout) return @{ Task = @{ State = "Ready" } } }
  function Get-OwnedHostProcesses { Param($Layout) $script:processQueries++; if ($script:processQueries -gt 1) { return $process } }
  function Enable-ScheduledTask { Param($TaskPath, $TaskName, $ErrorAction) }
  function Start-ScheduledTask { Param($TaskPath, $TaskName, $ErrorAction) }
  function Request-HostIndicator { Param($Process, $Close, $Timeout) $script:indicatorChecks++; return $true }
  function Start-Sleep { throw "end the first incomplete observation" }
  Rejects { & $script:realStart $script:layout } "accepted a visible process while task was Ready"
  Assert ($script:indicatorChecks -eq 0) "read manual indicator before live task evidence"
}

# Keep transaction and directory cleanup functions real. Substitute ONLY the
# external Windows operations, retaining task-definition ownership validation.
function Get-OwnedHostTask($Layout) {
  if (-not $script:taskXml) { return $null }
  Assert-OwnedHostTask $Layout $script:taskXml
  return @{ Xml = $script:taskXml; Task = @{ State = $script:taskState } }
}
function Stop-OwnedHost($Layout) {
  $script:stopCount++
  $script:events.Add("stop")
  if ($script:taskXml) { $script:taskXml = $script:taskXml.Replace("<Enabled>true</Enabled><Hidden>", "<Enabled>false</Enabled><Hidden>") }
  if ($script:failStopAt -eq $script:stopCount) { throw "uncertain original process" }
  $script:taskState = "Ready"
}
function Start-OwnedHost($Layout) {
  $script:events.Add("start")
  if ($script:failStart) { throw "native startup failed" }
  $script:taskState = "Running"
}
function Register-ScheduledTask {
  Param($TaskPath, $TaskName, $Xml, [switch]$Force, $ErrorAction)
  $script:events.Add("register")
  if ($script:failRegister) { $script:failRegister = $false; throw "registration refused" }
  $script:taskXml = $Xml
  $script:taskState = "Ready"
}
function Unregister-ScheduledTask {
  Param($TaskPath, $TaskName, [switch]$Confirm, $ErrorAction)
  $script:events.Add("unregister")
  $script:taskXml = $null
}
function Get-HostRegistration($Layout) { return $script:registration }
function Set-HostRegistration($Layout) {
  $script:events.Add("metadata")
  $script:registration = @{ Registry = @{ DisplayName = "new registration" }; Links = @{} }
  if ($script:failMetadata) { throw "shortcut publication failed" }
}
function Restore-HostRegistration($Layout, $Saved) { $script:events.Add("restore-metadata"); $script:registration = $Saved }

Test "missing host payload refuses without stopping existing host" {
  Scenario; Existing
  $missing = Join-Path $script:scratch "no-host"
  New-Item -ItemType Directory -Path $missing | Out-Null
  Rejects { Install-OwnedHost $script:layout $missing $false $true } "accepted missing payload"
  Assert ($script:events.Count -eq 0) "stopped a host before payload validation"
  Assert ((Get-Content -LiteralPath (Join-Path $script:layout.Current "frd.exe")).StartsWith("old-")) "old executable changed"
}

Test "staging copy failure preserves original task and untouched metadata" {
  Scenario; Existing
  $before = $script:taskXml
  $saved = $script:registration
  function Copy-Item { throw "staging copy failed" }
  Rejects { Install-OwnedHost $script:layout $script:payload $false $true } "ignored staging failure"
  Assert ($script:events.Count -eq 0) "staging failure stopped host or rewrote untouched registration"
  Assert ($script:taskXml -ceq $before) "task changed before staging succeeded"
  Assert ([Object]::ReferenceEquals($saved, $script:registration)) "metadata restoration ran unnecessarily"
  Assert (-not (Test-Path -LiteralPath $script:layout.Staging)) "owned partial staging retained"
  Assert ((Get-Content -LiteralPath (Join-Path $script:layout.Current "frd.exe")).StartsWith("old-")) "old files changed"
}

Test "staging manifest failure preserves old files without metadata writes" {
  Scenario; Existing
  function Set-Content { throw "staging manifest failed" }
  Rejects { Install-OwnedHost $script:layout $script:payload $false $true } "ignored manifest write failure"
  Assert ($script:events.Count -eq 0) "manifest failure mutated original lifecycle"
  Assert (-not (Test-Path -LiteralPath $script:layout.Staging)) "partial staged executables retained"
}

Test "first install publishes both real executable names without implicit launch" {
  Scenario
  Install-OwnedHost $script:layout $script:payload $false $false
  Assert ($script:events -join ',' -eq "stop,register,metadata") "unexpected lifecycle"
  foreach ($name in $script:HostFiles) { Assert (Test-Path -LiteralPath (Join-Path $script:layout.Current $name)) "missing installed $name" }
  Assert (-not (Test-Path -LiteralPath $script:layout.Previous)) "unbounded backup"
}

Test "upgrade retires original before replacing and starting" {
  Scenario; Existing
  Install-OwnedHost $script:layout $script:payload $true $true
  Assert ($script:events -join ',' -eq "stop,register,metadata,start") "wrong upgrade order"
  Assert ((Get-Content -LiteralPath (Join-Path $script:layout.Current "frd.exe")).StartsWith("new-")) "new payload not published"
  Assert ($script:taskXml.Contains(" --control")) "explicit control not installed"
  Assert (-not (Test-Path -LiteralPath $script:layout.Previous)) "old files retained after successful update"
}

Test "registration failure restores old payload and exact task without restarting" {
  Scenario; Existing
  $before = $script:taskXml
  $script:failRegister = $true
  Rejects { Install-OwnedHost $script:layout $script:payload $true $true } "ignored failed registration"
  Assert ($script:taskXml -ceq $before) "original task not restored"
  Assert ((Get-Content -LiteralPath (Join-Path $script:layout.Current "frd.exe")).StartsWith("old-")) "old payload not restored"
  Assert ($script:events -notcontains "start") "restarted during rollback"
}

Test "failed startup retires new owner before restoring original installation" {
  Scenario; Existing
  $before = $script:taskXml
  $script:failStart = $true
  Rejects { Install-OwnedHost $script:layout $script:payload $false $true } "ignored failed start"
  Assert ($script:stopCount -eq 2) "new owner not positively retired"
  Assert ($script:taskXml -ceq $before) "previous task not restored"
  Assert ((Get-Content -LiteralPath (Join-Path $script:layout.Current "fr-media-worker.exe")).StartsWith("old-")) "worker not restored"
  Assert ($script:registration.Registry.DisplayName -eq "FrankenRemote Host") "old uninstall metadata not restored"
}

Test "partial metadata failure restores previous task and registration" {
  Scenario; Existing
  $script:failMetadata = $true
  Rejects { Install-OwnedHost $script:layout $script:payload $false $true } "ignored metadata failure"
  Assert ($script:registration.Registry.DisplayName -eq "FrankenRemote Host") "partial metadata was retained"
  Assert ((Get-Content -LiteralPath (Join-Path $script:layout.Current "frd.exe")).StartsWith("old-")) "payload not restored"
}

Test "uncertain old retirement leaves original files and disables autostart" {
  Scenario; Existing
  $script:failStopAt = 1
  Rejects { Install-OwnedHost $script:layout $script:payload $false $true } "replaced uncertain original owner"
  Assert ($script:events -notcontains "register") "registered after uncertain old retirement"
  Assert ($script:taskXml.Contains("<Enabled>false</Enabled><Hidden>")) "old task resurrected"
  Assert ((Get-Content -LiteralPath (Join-Path $script:layout.Current "frd.exe")).StartsWith("old-")) "original files replaced"
}

Test "uncertain new retirement retains one backup and never restores a live old owner" {
  Scenario; Existing
  $script:failStart = $true; $script:failStopAt = 2
  Rejects { Install-OwnedHost $script:layout $script:payload $false $true } "ignored uncertain new retirement"
  Assert ($script:taskXml.Contains("<Enabled>false</Enabled><Hidden>")) "new task remains enabled"
  Assert ((Get-Content -LiteralPath (Join-Path $script:layout.Previous "frd.exe")).StartsWith("old-")) "rollback copy lost"
  Assert ((Get-Content -LiteralPath (Join-Path $script:layout.Current "frd.exe")).StartsWith("new-")) "replaced a possibly live new process"
  $count = $script:events.Count
  Rejects { Install-OwnedHost $script:layout $script:payload $false $true } "started another update over uncertain owner"
  Assert ($script:events.Count -eq $count) "repeat update mutated uncertain state"
}

Test "foreign task and unrelated program files are preserved" {
  Scenario; Existing
  $script:taskXml = $script:taskXml.Replace("InteractiveToken", "ServiceAccount")
  Rejects { Install-OwnedHost $script:layout $script:payload $false $true } "overwrote foreign task"
  Assert ($script:events.Count -eq 0) "foreign task mutated"
  Scenario; Existing
  $extra = Join-Path $script:layout.Current "personal-file.txt"
  Set-Content -LiteralPath $extra -Value "keep"
  Rejects { Install-OwnedHost $script:layout $script:payload $false $true } "replaced directory with unrelated files"
  Assert ((Get-Content -LiteralPath $extra) -eq "keep") "unrelated file changed"
}

Test "host uninstall confirms stop and preserves separate client" {
  Scenario; Existing
  $client = Join-Path (Split-Path -Parent $script:layout.Root) "FrankenRemote.exe"
  Set-Content -LiteralPath $client -Value "client"
  Uninstall-OwnedHost $script:layout
  Assert ($script:events -join ',' -eq "stop,unregister,restore-metadata") "wrong removal order"
  Assert (-not (Test-Path -LiteralPath $script:layout.Current)) "host executable retained"
  Assert ((Get-Content -LiteralPath $client) -eq "client") "client changed"
}

Test "PowerShell Host alias works without assigning read-only automatic variable" {
  $env:FR_INSTALL_LIB_ONLY = "1"
  . (Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) "install.ps1") -Host -NoLaunch
  Assert ([bool]$InstallHost) "-Host alias did not bind"
  Assert ($Host -is [System.Management.Automation.Host.PSHost]) "automatic Host variable was replaced"
  Assert-HostPayload $script:payload
}

Write-Host "$($script:passed) host installer policy/transaction tests passed. Fixtures: $script:scratch"
