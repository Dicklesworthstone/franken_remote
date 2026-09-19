# FrankenRemote Phase 0 OS Lifecycle Spike: Windows Session & Lifecycle Probe
# Usage: powershell -ExecutionPolicy Bypass -File spikes/os-lifecycle/windows/probe_session.ps1

Write-Host "=== FrankenRemote Windows OS Lifecycle & Session Probe ==="
$dateUtc = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
Write-Host "Timestamp (UTC): $dateUtc"
Write-Host "Computer:        $env:COMPUTERNAME"
Write-Host "OS Version:      $([System.Environment]::OSVersion.VersionString)"

Write-Host "`n--- Session Architecture & Integrity ---"
$currentProcess = [System.Diagnostics.Process]::GetCurrentProcess()
$currentSessionId = $currentProcess.SessionId
Write-Host "Current Process ID:         $($currentProcess.Id)"
Write-Host "Current Session ID:         $currentSessionId"

# Check Active Console Session ID via kernel32
$kernel32 = Add-Type -MemberDefinition @"
[DllImport("kernel32.dll")]
public static extern uint WTSGetActiveConsoleSessionId();
"@ -Name "Kernel32Helper" -Namespace "FrankenRemote.Probe" -PassThru

$activeConsoleSession = $kernel32::WTSGetActiveConsoleSessionId()
Write-Host "Active Console Session ID:  $activeConsoleSession"

if ($currentSessionId -eq 0) {
    Write-Host "WARNING: Running in Session 0 (Service Context)!"
    Write-Host "Desktop Duplication and interactive GUI capture are NOT supported directly from Session 0."
} else {
    Write-Host "Running in user interactive session ($currentSessionId)."
}

Write-Host "`n--- UIPI & Integrity Level ---"
$identity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [System.Security.Principal.WindowsPrincipal]$identity
$isAdmin = $principal.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)
Write-Host "User Account:               $($identity.Name)"
Write-Host "Is Administrator:           $isAdmin"
Write-Host "SendInput Limitation Note:  Ordinary medium-integrity processes cannot inject into elevated/UAC windows without uiAccess=true."

Write-Host "`n--- Power & Sleep Execution State ---"
Write-Host "SetThreadExecutionState (ES_SYSTEM_REQUIRED | ES_AWAYMODE_REQUIRED) prevents idle sleep during active streaming."

Write-Host "`n=== End of Windows Probe ==="
