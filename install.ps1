# FrankenRemote installer for Windows
#
#   irm https://raw.githubusercontent.com/Dicklesworthstone/franken_remote/main/install.ps1 | iex
#
# With options, run it as a script block:
#
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/Dicklesworthstone/franken_remote/main/install.ps1))) -NoLaunch
#
# or set the matching FR_INSTALL_* environment variable before the one-liner
# (for example FR_INSTALL_VERSION=v0.2.0 or FR_INSTALL_YES=1).
#
# What it does, for the current user only (no administrator rights, except
# for installing Tailscale itself):
#   - checks Windows 10 1809+ on x64, PowerShell 5.1+, Tailscale and the HEVC
#     video decoder, and offers to install the missing pieces
#   - downloads the latest release, verified with SHA-256 and, when cosign is
#     installed, its Sigstore bundle; with no release it can build from source
#   - installs the FrankenRemote desktop app and the fr-windows command line to
#     %LOCALAPPDATA%\Programs\FrankenRemote with a Start-menu shortcut, an
#     Apps & features entry and a user PATH entry
#   - installs an agent skill for Claude Code / Codex when they are present
#   - opens the app, which lists your Tailscale machines
#
# Options:
#   -Version vX.Y.Z   Install this release (default: latest)
#   -FromSource       Build from source (rustup + the WinLibs GCC toolchain)
#   -Ref REF          Source branch, tag or commit for -FromSource (default: main)
#   -Offline ZIP      Install a local release zip (SHA256SUMS beside it, or -Checksum)
#   -ArtifactUrl URL  Download this release zip instead of the default
#   -Checksum HEX     Expected SHA-256 of the zip
#   -NoVerify         Skip checksum and signature checks (testing only)
#   -NoLaunch         Do not open the app afterwards
#   -NoTailscale      Do not offer to install Tailscale
#   -NoConfigure      Skip the agent skill
#   -Yes              Answer yes to every prompt
#   -Force            Reinstall even when the same build is installed
#   -Uninstall        Remove FrankenRemote (the app's own uninstall)
#   -Quiet            Errors, warnings and the result only
#   -Help             Show this help

Param(
  [string]$Version = $env:FR_INSTALL_VERSION,
  [string]$Ref = $env:FR_INSTALL_REF,
  [switch]$FromSource,
  [string]$Offline = $env:FR_INSTALL_OFFLINE,
  [string]$ArtifactUrl = $env:FR_INSTALL_ARTIFACT_URL,
  [string]$Checksum = $env:FR_INSTALL_CHECKSUM,
  [switch]$NoVerify,
  [switch]$NoLaunch,
  [switch]$NoTailscale,
  [switch]$NoConfigure,
  [switch]$Yes,
  [switch]$Force,
  [switch]$Uninstall,
  [switch]$Quiet,
  [switch]$Help,
  [string]$Owner = "Dicklesworthstone",
  [string]$Repo = "franken_remote"
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

# Switches can also come from the environment, for the plain `irm | iex` form.
function Test-EnvFlag([string]$Name) {
  $value = [Environment]::GetEnvironmentVariable($Name)
  return ($value -eq "1" -or $value -eq "true" -or $value -eq "yes")
}
if (Test-EnvFlag "FR_INSTALL_FROM_SOURCE") { $FromSource = $true }
if (Test-EnvFlag "FR_INSTALL_NO_LAUNCH") { $NoLaunch = $true }
if (Test-EnvFlag "FR_INSTALL_NO_TAILSCALE") { $NoTailscale = $true }
if (Test-EnvFlag "FR_INSTALL_NO_CONFIGURE") { $NoConfigure = $true }
if (Test-EnvFlag "FR_INSTALL_YES") { $Yes = $true }
if (Test-EnvFlag "FR_INSTALL_FORCE") { $Force = $true }
if (Test-EnvFlag "FR_INSTALL_UNINSTALL") { $Uninstall = $true }
if (Test-EnvFlag "FR_INSTALL_QUIET") { $Quiet = $true }
if ($Ref) { $FromSource = $true }

# Windows PowerShell 5.1 may still default to TLS 1.0/1.1, which GitHub refuses.
try {
  [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch { }

$script:Product = "FrankenRemote"
$script:RepoUrl = "https://github.com/$Owner/$Repo"
$script:RawBase = "https://raw.githubusercontent.com/$Owner/$Repo"
$script:Target = "x86_64-pc-windows-gnu"
$script:InstallDir = $null
$script:CacheDir = $null
$script:UninstallKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\FrankenRemote"
$script:HevcStore = "ms-windows-store://pdp/?ProductId=9NMZLZ57R3T7"
$script:CosignIdentity = "^https://github.com/$Owner/$Repo/.github/workflows/release.yml@refs/tags/.*$"
$script:CosignIssuer = "https://token.actions.githubusercontent.com"
$script:Interactive = [Environment]::UserInteractive -and -not ([Environment]::GetCommandLineArgs() -match '^-NonI')

# The two public roots the Windows build embeds (fr-mobile-ffi build_support/roots.rs).
$script:IsrgRoots = @(
  @{ File = "isrg-root-x1.pem"; Url = "https://letsencrypt.org/certs/isrgrootx1.pem"; Sha256 = "96bcec06264976f37460779acf28c5a7cfe8a3c0aae11a8ffcee05c0bddf08c6" },
  @{ File = "isrg-root-x2.pem"; Url = "https://letsencrypt.org/certs/isrg-root-x2.pem"; Sha256 = "69729b8e15a86efc177a57afb7171dfc64add28c2fca8cf1507e34453ccb1470" }
)

# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------

function Write-Info([string]$Message) { if (-not $Quiet) { Write-Host "-> $Message" -ForegroundColor Cyan } }
function Write-Ok([string]$Message) { if (-not $Quiet) { Write-Host "[ok] $Message" -ForegroundColor Green } }
function Write-Warn([string]$Message) { Write-Host "[!] $Message" -ForegroundColor Yellow }
function Write-Err([string]$Message) { Write-Host "[x] $Message" -ForegroundColor Red }

# A double-line box sized to the widest line. The characters are built from
# code points so this file stays ASCII (Windows PowerShell 5.1 reads a
# BOM-less script file in the ANSI code page).
function Show-Box([string[]]$Lines, [string]$Color = "Cyan") {
  $h = [string][char]0x2550; $v = [string][char]0x2551
  $tl = [string][char]0x2554; $tr = [string][char]0x2557; $bl = [string][char]0x255A; $br = [string][char]0x255D
  $width = 0
  foreach ($line in $Lines) { if ($line.Length -gt $width) { $width = $line.Length } }
  Write-Host ($tl + ($h * ($width + 4)) + $tr) -ForegroundColor $Color
  foreach ($line in $Lines) {
    Write-Host $v -ForegroundColor $Color -NoNewline
    Write-Host ("  " + $line.PadRight($width) + "  ") -NoNewline
    Write-Host $v -ForegroundColor $Color
  }
  Write-Host ($bl + ($h * ($width + 4)) + $br) -ForegroundColor $Color
}

function Show-Header {
  if ($Quiet) { return }
  Write-Host ""
  Show-Box @("FrankenRemote installer", "", "Remote desktop over your tailnet", "Tailscale identity, hardware HEVC, no accounts") "Cyan"
  Write-Host ""
}

function Show-Usage {
  $text = @'
Usage: install.ps1 [options]

  irm https://raw.githubusercontent.com/Dicklesworthstone/franken_remote/main/install.ps1 | iex

Installs the FrankenRemote desktop app (FrankenRemote.exe) and command line
(fr-windows.exe) for the current user.

  -Version vX.Y.Z   Install this release (default: the latest release)
  -FromSource       Build from source; installs rustup and the WinLibs GCC
                    toolchain with winget after asking (about 3 GB, 20-60 min)
  -Ref REF          Source branch, tag or commit (implies -FromSource)
  -Offline ZIP      Install a local release zip (no download)
  -ArtifactUrl URL  Download this release zip instead of the default
  -Checksum HEX     Expected SHA-256 of the zip
  -NoVerify         Skip checksum and signature checks (testing only)
  -NoLaunch         Do not open the app afterwards
  -NoTailscale      Do not offer to install Tailscale
  -NoConfigure      Skip the agent skill for Claude Code / Codex
  -Yes              Answer yes to every prompt
  -Force            Reinstall even when the same build is already installed
  -Uninstall        Remove FrankenRemote (shortcut, Apps & features, PATH, files)
  -Quiet            Print errors, warnings and the result only
  -Help             Show this help

Environment: FR_INSTALL_VERSION, FR_INSTALL_REF, FR_INSTALL_OFFLINE,
FR_INSTALL_ARTIFACT_URL, FR_INSTALL_CHECKSUM, and FR_INSTALL_FROM_SOURCE,
FR_INSTALL_NO_LAUNCH, FR_INSTALL_NO_TAILSCALE, FR_INSTALL_NO_CONFIGURE,
FR_INSTALL_YES, FR_INSTALL_FORCE, FR_INSTALL_UNINSTALL, FR_INSTALL_QUIET (=1).
HTTPS_PROXY is honoured; otherwise the Windows proxy settings apply.
'@
  Write-Host $text
}

# Yes/no prompt; -Yes answers yes and a non-interactive session takes the default.
function Read-YesNo([string]$Prompt, [bool]$Default = $false) {
  if ($Yes) { return $true }
  if (-not $script:Interactive) { return $Default }
  $hint = "[y/N]"; if ($Default) { $hint = "[Y/n]" }
  try { $reply = Read-Host "$Prompt $hint" } catch { return $Default }
  if ([string]::IsNullOrWhiteSpace($reply)) { return $Default }
  return ($reply.Trim() -match '^(y|yes)$')
}

# ---------------------------------------------------------------------------
# Network and verification
# ---------------------------------------------------------------------------

function Get-ProxyArgs {
  $proxy = $env:HTTPS_PROXY; if (-not $proxy) { $proxy = $env:https_proxy }
  if (-not $proxy) { $proxy = $env:HTTP_PROXY }; if (-not $proxy) { $proxy = $env:http_proxy }
  if ($proxy) { return @{ Proxy = $proxy } }
  return @{}
}

function Invoke-Download([string]$Url, [string]$OutFile) {
  $proxyArgs = Get-ProxyArgs
  $attempt = 0
  while ($true) {
    $attempt++
    try {
      Invoke-WebRequest -Uri $Url -OutFile $OutFile -UseBasicParsing -TimeoutSec 300 @proxyArgs
      return
    } catch {
      $status = $null
      try { $status = [int]$_.Exception.Response.StatusCode } catch { }
      if ($status -eq 404 -or $attempt -ge 3) { throw }
      Start-Sleep -Seconds (2 * $attempt)
    }
  }
}

function Test-Sha256Token([string]$Token) { return ($Token -match '^[0-9A-Fa-f]{64}$') }

function Get-Sha256([string]$Path) {
  try {
    $hash = (Get-FileHash -LiteralPath $Path -Algorithm SHA256 -ErrorAction Stop).Hash
    if (Test-Sha256Token $hash) { return $hash.ToLowerInvariant() }
  } catch { }
  # Get-FileHash is refused under WDAC ConstrainedLanguage; certutil is inbox.
  $lines = @(& certutil.exe -hashfile $Path SHA256 2>$null)
  foreach ($line in $lines) {
    $token = ([string]$line).Replace(" ", "").Trim()
    if (Test-Sha256Token $token) { return $token.ToLowerInvariant() }
  }
  throw "Cannot compute the SHA-256 of $Path"
}

function Assert-Checksum([string]$Path, [string]$Expected) {
  $actual = Get-Sha256 $Path
  if ($actual -ne $Expected.ToLowerInvariant()) {
    # Never delete it here: an -Offline zip is the user's own file, and
    # downloads live in the temporary folder removed on exit.
    throw "Checksum verification FAILED for $(Split-Path -Leaf $Path) (expected $Expected, got $actual)"
  }
  Write-Ok "Checksum verified: $($actual.Substring(0, 16))..."
}

# The expected digest of $Name from a SHA256SUMS file.
function Find-ManifestChecksum([string]$ManifestPath, [string]$Name) {
  foreach ($line in Get-Content -LiteralPath $ManifestPath) {
    $parts = ($line.Trim() -split '\s+', 2)
    if ($parts.Count -eq 2 -and (Test-Sha256Token $parts[0]) -and ($parts[1].TrimStart('*') -ceq $Name)) {
      return $parts[0].ToLowerInvariant()
    }
  }
  return $null
}

# cosign absent or too old: skip with a note. Present and a bundle that fails: stop.
function Test-CosignVersion([string]$Text) {
  if ($Text -notmatch 'v?(\d+)\.(\d+)\.(\d+)') { return $false }
  $major = [int]$Matches[1]; $minor = [int]$Matches[2]; $patch = [int]$Matches[3]
  if ($major -ge 4) { return $true }
  if ($major -eq 3) { return ($minor -gt 0 -or $patch -ge 4) }
  if ($major -eq 2) { return ($minor -gt 6 -or ($minor -eq 6 -and $patch -ge 2)) }
  return $false
}

function Invoke-SigstoreCheck([string]$ArtifactPath, [string]$BundleUrl, [string]$TempDir) {
  $cosign = Get-Command cosign -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
  if (-not $cosign) { Write-Info "cosign not found; skipping the Sigstore signature check (the checksum was verified)"; return }
  $bundle = Join-Path $TempDir "bundle.sigstore.json"
  try { Invoke-Download $BundleUrl $bundle } catch { Write-Warn "No Sigstore bundle published for this artifact; skipping the signature check"; return }
  $saved = $ErrorActionPreference
  try {
    $ErrorActionPreference = "Continue"
    $versionText = (& $cosign.Source version 2>&1 | Out-String)
    if (-not (Test-CosignVersion $versionText)) {
      Write-Warn "cosign is older than 2.6.2 / 3.0.4 (bundle verification fixes); skipping the signature check"
      return
    }
    $global:LASTEXITCODE = -1
    $output = (& $cosign.Source verify-blob --bundle $bundle --certificate-identity-regexp $script:CosignIdentity --certificate-oidc-issuer $script:CosignIssuer $ArtifactPath 2>&1 | Out-String)
    $code = $global:LASTEXITCODE
  } finally { $ErrorActionPreference = $saved }
  if ($code -ne 0) { throw "The Sigstore bundle does not verify against the release workflow identity: $($output.Trim())" }
  Write-Ok "Signature verified (Sigstore, $Owner/$Repo release workflow)"
}

# ---------------------------------------------------------------------------
# Platform and prerequisites
# ---------------------------------------------------------------------------

function Assert-Platform {
  if ($PSVersionTable.PSVersion.Major -ge 6 -and -not $IsWindows) {
    throw "This installer is for Windows. On macOS or Linux run: curl -fsSL $script:RawBase/main/install.sh | bash"
  }
  if ($PSVersionTable.PSVersion -lt [Version]"5.1") { throw "PowerShell 5.1 or newer is required" }
  $build = [Environment]::OSVersion.Version.Build
  if ($build -lt 17763) { throw "Windows 10 version 1809 (build 17763) or newer is required (this is build $build)" }
  $arch = $env:PROCESSOR_ARCHITEW6432; if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
  switch ($arch) {
    "AMD64" { }
    "ARM64" { Write-Warn "Windows on ARM: the x64 build runs under emulation (GPU decode depends on the ARM driver)" }
    default { throw "FrankenRemote needs 64-bit Windows (this is $arch)" }
  }
  if (-not $env:LOCALAPPDATA) { throw "LOCALAPPDATA is not set; run the installer as an ordinary signed-in user" }
  $script:InstallDir = Join-Path $env:LOCALAPPDATA "Programs\FrankenRemote"
  $script:CacheDir = Join-Path $env:LOCALAPPDATA "FrankenRemote\installer-cache"
  $freeGb = Get-FreeGigabytes $env:LOCALAPPDATA
  if ($null -ne $freeGb -and $freeGb -lt 1) { throw "Less than 1 GB is free on the drive holding $env:LOCALAPPDATA" }
  Write-Ok "Windows build $build, $arch, PowerShell $($PSVersionTable.PSVersion)"
}

function Get-TailscaleExe {
  $command = Get-Command tailscale.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
  if ($command) { return $command.Source }
  foreach ($dir in @($env:ProgramFiles, ${env:ProgramFiles(x86)})) {
    if ($dir) {
      $candidate = Join-Path $dir "Tailscale\tailscale.exe"
      if (Test-Path -LiteralPath $candidate) { return $candidate }
    }
  }
  return $null
}

$script:TailscaleState = "missing"
function Test-Tailscale {
  $exe = Get-TailscaleExe
  if (-not $exe) {
    Write-Warn "Tailscale is not installed. FrankenRemote uses it for every connection and identity."
    if ($NoTailscale) { return }
    $winget = Get-Command winget.exe -ErrorAction SilentlyContinue
    if ($winget -and (Read-YesNo "Install Tailscale with winget (Windows asks for administrator approval)?" $true)) {
      & $winget.Source install --exact --id Tailscale.Tailscale --accept-source-agreements --accept-package-agreements --silent
      $exe = Get-TailscaleExe
      if ($exe) { Write-Ok "Tailscale installed; sign in from its icon in the notification area" }
    }
    if (-not $exe) { Write-Info "Install Tailscale from https://tailscale.com/download/windows, then sign in"; return }
  }
  $script:TailscaleState = "installed"
  try {
    $saved = $ErrorActionPreference; $ErrorActionPreference = "Continue"
    $json = (& $exe status --json 2>$null | Out-String)
    $ErrorActionPreference = $saved
    $state = ($json | ConvertFrom-Json).BackendState
    if ($state -eq "Running") { $script:TailscaleState = "running"; Write-Ok "Tailscale is running" }
    else { Write-Warn "Tailscale is installed but not connected ($state); sign in from its notification-area icon" }
  } catch { Write-Warn "Tailscale is installed; its state could not be read" }
}

# The app itself reports whether Media Foundation offers an HEVC decoder; the
# Store package is the usual provider, so check for it up front.
$script:HevcState = "unknown"
function Test-HevcDecoder {
  $package = $null
  try { $package = Get-AppxPackage -Name "Microsoft.HEVCVideoExtension*" -ErrorAction Stop | Select-Object -First 1 } catch { }
  if ($package) { $script:HevcState = "installed"; Write-Ok "HEVC Video Extensions $($package.Version)"; return }
  $script:HevcState = "missing"
  Write-Warn "The HEVC Video Extensions were not found. Without an HEVC decoder FrankenRemote cannot show a remote desktop."
  Write-Warn "Some PCs ship an equivalent decoder from the manufacturer; the app's status line says which decoder it found."
  if (Read-YesNo "Open the HEVC Video Extensions page in the Microsoft Store?" $true) {
    Start-Process $script:HevcStore
  }
}

# ---------------------------------------------------------------------------
# Release
# ---------------------------------------------------------------------------

function ConvertTo-Tag([string]$Value) {
  if (-not $Value) { return $null }
  if (-not $Value.StartsWith("v")) { $Value = "v$Value" }
  if ($Value -match '^v\d+\.\d+\.\d+([-.][0-9A-Za-z.-]+)?$') { return $Value }
  throw "Invalid version '$Value' (expected vX.Y.Z)"
}

function Resolve-LatestTag {
  $proxyArgs = Get-ProxyArgs
  try {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Owner/$Repo/releases/latest" -Headers @{ Accept = "application/vnd.github+json" } -UseBasicParsing -TimeoutSec 30 @proxyArgs
    if ($release.tag_name) { return (ConvertTo-Tag $release.tag_name) }
  } catch { }
  try {
    # The API is rate limited; the latest-release page redirects to its tag.
    $response = Invoke-WebRequest -Uri "$script:RepoUrl/releases/latest" -UseBasicParsing -TimeoutSec 30 @proxyArgs
    $final = $response.BaseResponse.ResponseUri
    if (-not $final) { $final = $response.BaseResponse.RequestMessage.RequestUri }
    if ($final -and ($final.AbsoluteUri -match '/releases/tag/([^/?#]+)')) { return (ConvertTo-Tag $Matches[1]) }
  } catch { }
  return $null
}

# Exactly one top-level folder holding plain files, no traversal or links.
function Assert-ZipLayout([string]$ZipPath) {
  $names = @()
  if ($ExecutionContext.SessionState.LanguageMode -eq "FullLanguage") {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($ZipPath)
    try {
      foreach ($entry in $zip.Entries) {
        $unixType = (($entry.ExternalAttributes -shr 16) -band 0xF000)
        if ($unixType -ne 0 -and $unixType -ne 0x8000 -and $unixType -ne 0x4000) { throw "The release zip contains a link or special file: $($entry.FullName)" }
        $names += $entry.FullName
      }
    } finally { $zip.Dispose() }
  } else {
    # WDAC ConstrainedLanguage: inspect with the inbox bsdtar instead of .NET.
    $tar = Join-Path $env:SystemRoot "System32\tar.exe"
    if (-not (Test-Path -LiteralPath $tar)) { throw "Cannot inspect the release zip: tar.exe is unavailable" }
    $details = @(& $tar -tvf $ZipPath)
    if ($LASTEXITCODE -ne 0) { throw "tar.exe could not read the release zip" }
    foreach ($line in $details) {
      $kind = ([string]$line).TrimStart()
      if ($kind -and -not ($kind.StartsWith("-") -or $kind.StartsWith("d"))) { throw "The release zip contains a link or special file: $line" }
    }
    $names = @(& $tar -tf $ZipPath | Where-Object { $_ })
  }
  $top = $null
  foreach ($raw in $names) {
    $name = $raw -replace '\\', '/'
    if ($name.StartsWith("/") -or $name -match '(^|/)\.\.(/|$)' -or $name -match '^[A-Za-z]:') { throw "Unsafe path in the release zip: $name" }
    $first = $name.Split('/')[0]
    if ($null -eq $top) { $top = $first } elseif ($first -ne $top) { throw "The release zip has more than one top-level folder" }
  }
  if (-not $top) { throw "The release zip is empty" }
  return $top
}

function Expand-Release([string]$ZipPath, [string]$TempDir) {
  $top = Assert-ZipLayout $ZipPath
  $out = Join-Path $TempDir "unpacked"
  Expand-Archive -LiteralPath $ZipPath -DestinationPath $out -Force
  $payload = Join-Path $out $top
  foreach ($name in @("FrankenRemote.exe", "fr-windows.exe")) {
    if (-not (Test-Path -LiteralPath (Join-Path $payload $name) -PathType Leaf)) { throw "The release zip has no $name" }
  }
  Get-ChildItem -LiteralPath $payload -Recurse -File | Unblock-File -ErrorAction SilentlyContinue
  $commitFile = Join-Path $payload "COMMIT"
  if (Test-Path -LiteralPath $commitFile) { $script:Commit = (Get-Content -LiteralPath $commitFile -TotalCount 1).Trim() }
  return $payload
}

function Get-ReleasePayload([string]$TempDir) {
  if ($Offline) {
    if (-not (Test-Path -LiteralPath $Offline -PathType Leaf)) { throw "Offline zip not found: $Offline" }
    $zip = (Resolve-Path -LiteralPath $Offline).Path
    $name = Split-Path -Leaf $zip
    $expected = $Checksum
    if (-not $expected) {
      $sums = Join-Path (Split-Path -Parent $zip) "SHA256SUMS"
      if (Test-Path -LiteralPath $sums) { $expected = Find-ManifestChecksum $sums $name }
    }
    if (-not $NoVerify) {
      if ($expected) { Assert-Checksum $zip $expected }
      elseif (-not (Read-YesNo "No checksum for $name (pass -Checksum or keep SHA256SUMS beside it). Install it unverified?" $false)) {
        throw "Aborted: unverified offline zip"
      }
    }
    $script:InstalledVersion = $name -replace '\.zip$', ''
    $script:InstalledSource = "offline zip"
    return (Expand-Release $zip $TempDir)
  }

  if ($ArtifactUrl) {
    $url = $ArtifactUrl
    $name = Split-Path -Leaf (($ArtifactUrl -split '\?')[0])
  } else {
    $name = "franken_remote-$($script:Tag)-$($script:Target).zip"
    $url = "$script:RepoUrl/releases/download/$($script:Tag)/$name"
  }
  $zip = Join-Path $TempDir $name
  Write-Info "Downloading $name"
  try { Invoke-Download $url $zip } catch { Write-Warn "No Windows release artifact at $url"; return $null }
  if ($NoVerify) {
    Write-Warn "Verification skipped (-NoVerify)"
  } else {
    $expected = $Checksum
    if (-not $expected) {
      $base = $url.Substring(0, $url.LastIndexOf('/'))
      $sums = Join-Path $TempDir "SHA256SUMS"
      try { Invoke-Download "$base/SHA256SUMS" $sums; $expected = Find-ManifestChecksum $sums $name } catch { }
      if (-not $expected) { throw "The release publishes no SHA-256 for $name (use -Checksum, or -NoVerify for testing)" }
    }
    Assert-Checksum $zip $expected
    Invoke-SigstoreCheck $zip "$url.sigstore.json" $TempDir
  }
  $script:InstalledVersion = $script:Tag
  if ($ArtifactUrl) { $script:InstalledVersion = $name -replace '\.zip$', '' }
  $script:InstalledSource = "release"
  return (Expand-Release $zip $TempDir)
}

# ---------------------------------------------------------------------------
# Source build
# ---------------------------------------------------------------------------

function Get-FreeGigabytes([string]$Path) {
  try {
    $drive = Get-PSDrive -Name ([IO.Path]::GetPathRoot($Path).TrimEnd('\', ':')) -PSProvider FileSystem -ErrorAction Stop
    return [Math]::Floor($drive.Free / 1GB)
  } catch { return $null }
}

function Update-SessionPath {
  $machine = [Environment]::GetEnvironmentVariable("Path", "Machine")
  $user = [Environment]::GetEnvironmentVariable("Path", "User")
  $env:Path = (@($machine, $user, $env:Path) | Where-Object { $_ }) -join ";"
}

function Find-Gcc {
  $gcc = Get-Command gcc.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
  if ($gcc) { return $gcc.Source }
  $root = Join-Path $env:LOCALAPPDATA "Microsoft\WinGet\Packages"
  if (Test-Path -LiteralPath $root) {
    $found = Get-ChildItem -LiteralPath $root -Directory -Filter "BrechtSanders.WinLibs*" -ErrorAction SilentlyContinue |
      ForEach-Object { Join-Path $_.FullName "mingw64\bin\gcc.exe" } | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
    if ($found) { return $found }
  }
  return $null
}

function Install-BuildTools([string]$TempDir) {
  $winget = Get-Command winget.exe -ErrorAction SilentlyContinue
  $cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
  if (-not (Get-Command rustup.exe -ErrorAction SilentlyContinue)) {
    if (Test-Path -LiteralPath (Join-Path $cargoBin "rustup.exe")) {
      $env:Path = "$cargoBin;$env:Path"
    } else {
      if (-not (Read-YesNo "Install rustup (the Rust toolchain manager) into $cargoBin?" $true)) { throw "rustup is required to build from source" }
      $url = "https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe"
      $init = Join-Path $TempDir "rustup-init.exe"
      Invoke-Download $url $init
      Invoke-Download "$url.sha256" "$init.sha256"
      Assert-Checksum $init ((Get-Content -LiteralPath "$init.sha256" -TotalCount 1).Trim().Split(' ')[0])
      # The GNU host toolchain matches the release artifact and needs no Visual Studio.
      & $init -y --default-host x86_64-pc-windows-gnu --default-toolchain none --profile minimal --no-modify-path
      if ($LASTEXITCODE -ne 0) { throw "rustup-init failed" }
      $env:Path = "$cargoBin;$env:Path"
      Write-Ok "rustup installed"
    }
  }
  $gcc = Find-Gcc
  if (-not $gcc) {
    if (-not $winget) { throw "A MinGW-w64 GCC is required (winget is unavailable; install WinLibs from https://winlibs.com and add its bin folder to PATH)" }
    if (-not (Read-YesNo "Install the WinLibs MinGW-w64 GCC toolchain with winget (about 1 GB)?" $true)) { throw "GCC is required to build from source" }
    & $winget.Source install --exact --id BrechtSanders.WinLibs.POSIX.UCRT --accept-source-agreements --accept-package-agreements --silent
    Update-SessionPath
    $gcc = Find-Gcc
    if (-not $gcc) { throw "GCC was installed but not found; open a new PowerShell and run the installer again" }
  }
  $gccDir = Split-Path -Parent $gcc
  if (($env:Path -split ';') -notcontains $gccDir) { $env:Path = "$gccDir;$env:Path" }
  Write-Ok "Build tools: rustup, $gcc"
}

# PEM text for a DER certificate.
function ConvertTo-Pem([byte[]]$Der) {
  $base64 = [Convert]::ToBase64String($Der)
  $lines = for ($i = 0; $i -lt $base64.Length; $i += 64) { $base64.Substring($i, [Math]::Min(64, $base64.Length - $i)) }
  return "-----BEGIN CERTIFICATE-----`n" + ($lines -join "`n") + "`n-----END CERTIFICATE-----`n"
}

function Get-DerSha256([byte[]]$Der) {
  $sha = [System.Security.Cryptography.SHA256]::Create()
  try { return (($sha.ComputeHash($Der) | ForEach-Object { $_.ToString("x2") }) -join "") } finally { $sha.Dispose() }
}

# The Windows build embeds the two ISRG roots and checks their pinned DER
# digests itself. Take them from this PC's root store, else from Let's Encrypt;
# either way the digests are checked here first.
function Get-IsrgRoots([string]$Dir) {
  New-Item -ItemType Directory -Force -Path $Dir | Out-Null
  $store = @()
  try { $store = @(Get-ChildItem -Path Cert:\LocalMachine\Root, Cert:\CurrentUser\Root -ErrorAction Stop) } catch { }
  foreach ($root in $script:IsrgRoots) {
    $der = $null
    foreach ($cert in $store) {
      if ((Get-DerSha256 $cert.RawData) -eq $root.Sha256) { $der = $cert.RawData; break }
    }
    if (-not $der) {
      $pemPath = Join-Path $Dir ($root.File + ".download")
      Invoke-Download $root.Url $pemPath
      $body = ((Get-Content -LiteralPath $pemPath -Raw) -replace '-----[A-Z ]+-----', '' -replace '\s', '')
      $der = [Convert]::FromBase64String($body)
      Remove-Item -LiteralPath $pemPath -Force
    }
    if ((Get-DerSha256 $der) -ne $root.Sha256) { throw "$($root.File) does not match its pinned digest" }
    [System.IO.File]::WriteAllText((Join-Path $Dir $root.File), (ConvertTo-Pem $der), (New-Object System.Text.UTF8Encoding $false))
  }
  Write-Ok "ISRG roots verified against the build's pinned digests"
}

function Build-FromSource([string]$TempDir) {
  $sourceRef = $Ref
  if (-not $sourceRef) { if ($script:Tag) { $sourceRef = $script:Tag } else { $sourceRef = "main" } }
  Write-Warn "A source build installs rustup and a MinGW-w64 GCC toolchain if they are missing, needs about 3 GB, and takes 20-60 minutes."
  if (-not (Read-YesNo "Build FrankenRemote from source ($sourceRef)?" $true)) { throw "Cancelled" }
  $freeGb = Get-FreeGigabytes $env:LOCALAPPDATA
  if ($null -ne $freeGb -and $freeGb -lt 10) {
    if (-not (Read-YesNo "Only $freeGb GB is free on the drive holding $env:LOCALAPPDATA; a source build needs about 10 GB. Build anyway?" $false)) { throw "Not enough disk space for a source build" }
  }
  Install-BuildTools $TempDir
  $proxyArgs = Get-ProxyArgs
  $sha = (Invoke-RestMethod -Uri "https://api.github.com/repos/$Owner/$Repo/commits/$sourceRef" -Headers @{ Accept = "application/vnd.github.sha" } -UseBasicParsing @proxyArgs)
  $sha = ([string]$sha).Trim()
  if ($sha -notmatch '^[0-9a-f]{40}$') { throw "Could not resolve $sourceRef to a commit" }
  $script:Commit = $sha
  New-Item -ItemType Directory -Force -Path $script:CacheDir | Out-Null
  $src = Join-Path $script:CacheDir "src-$($sha.Substring(0, 12))"
  if (-not (Test-Path -LiteralPath (Join-Path $src "Cargo.toml"))) {
    $zip = Join-Path $TempDir "source.zip"
    Write-Info "Downloading the source at $($sha.Substring(0, 10))"
    Invoke-Download "https://codeload.github.com/$Owner/$Repo/zip/$sha" $zip
    $unpack = Join-Path $TempDir "source"
    Expand-Archive -LiteralPath $zip -DestinationPath $unpack -Force
    $top = Get-ChildItem -LiteralPath $unpack -Directory | Select-Object -First 1
    Move-Item -LiteralPath $top.FullName -Destination $src
  }
  $roots = Join-Path $script:CacheDir "isrg-roots"
  Get-IsrgRoots $roots
  $env:FR_MOBILE_ISRG_ROOTS_DIR = $roots
  $env:CARGO_TARGET_DIR = Join-Path $script:CacheDir "target"
  # The git CLI fetches the git dependency far faster than cargo's own client.
  if (Get-Command git.exe -ErrorAction SilentlyContinue) { $env:CARGO_NET_GIT_FETCH_WITH_CLI = "true" }
  # The pinned nightly with a GNU host, whatever rustup's default host is: an
  # MSVC host would need Visual Studio's linker for build scripts.
  $toolchainFile = Get-Content -LiteralPath (Join-Path $src "rust-toolchain.toml") -Raw
  if ($toolchainFile -notmatch '(?m)^\s*channel\s*=\s*"([^"]+)"') { throw "rust-toolchain.toml names no channel" }
  $toolchain = "$($Matches[1])-x86_64-pc-windows-gnu"
  Push-Location $src
  try {
    Write-Info "Installing the pinned Rust toolchain ($toolchain)"
    & rustup.exe toolchain install $toolchain --profile minimal
    if ($LASTEXITCODE -ne 0) { throw "rustup could not install $toolchain" }
    Write-Info "Building FrankenRemote (release profile; later updates reuse $($env:CARGO_TARGET_DIR))"
    & cargo.exe "+$toolchain" build --release --locked -p fr-windows --bin FrankenRemote --bin fr-windows
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
  } finally { Pop-Location }
  $script:InstalledVersion = "source:$sourceRef@$($sha.Substring(0, 10))"
  $script:InstalledSource = "source"
  return (Join-Path $env:CARGO_TARGET_DIR "release")
}

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------

function Test-SameBuild([string]$Payload) {
  foreach ($name in @("FrankenRemote.exe", "fr-windows.exe")) {
    $installed = Join-Path $script:InstallDir $name
    if (-not (Test-Path -LiteralPath $installed)) { return $false }
    if ((Get-Sha256 $installed) -ne (Get-Sha256 (Join-Path $Payload $name))) { return $false }
  }
  return $true
}

function Stop-RunningCopies {
  $running = @(Get-Process -Name FrankenRemote, fr-windows -ErrorAction SilentlyContinue | Where-Object {
      try { $_.Path -and $_.Path.StartsWith($script:InstallDir, [StringComparison]::OrdinalIgnoreCase) } catch { $false }
    })
  if ($running.Count -eq 0) { return }
  Write-Warn "FrankenRemote is running ($($running.Count) process(es)); it must close before its files are replaced."
  if (-not (Read-YesNo "Close it now? Open remote sessions end." $true)) { throw "Close FrankenRemote and run the installer again" }
  $running | Stop-Process -Force
  Start-Sleep -Milliseconds 500
}

# FrankenRemote.exe performs the per-user installation itself.
function Install-Payload([string]$Payload) {
  Stop-RunningCopies
  $app = Join-Path $Payload "FrankenRemote.exe"
  $process = Start-Process -FilePath $app -ArgumentList "--install", "--quiet" -Wait -PassThru
  if ($process.ExitCode -ne 0) { throw "FrankenRemote --install failed (exit $($process.ExitCode))" }
  foreach ($name in @("FrankenRemote.exe", "fr-windows.exe")) {
    if (-not (Test-Path -LiteralPath (Join-Path $script:InstallDir $name))) { throw "$name is missing from $($script:InstallDir) after installation" }
  }
  if (-not (Test-Path -LiteralPath $script:UninstallKey)) { throw "The Apps & features entry was not registered" }
  Update-SessionPath
  Write-Ok "Installed to $($script:InstallDir) (Start menu, Apps & features, user PATH)"
}

$script:MachineCount = $null
function Test-InstalledCli {
  $cli = Join-Path $script:InstallDir "fr-windows.exe"
  try {
    $saved = $ErrorActionPreference; $ErrorActionPreference = "Continue"
    $text = (& $cli hosts 2>$null | Out-String)
    $ErrorActionPreference = $saved
    $doc = $text | ConvertFrom-Json
    if ($doc.machines) { $script:MachineCount = @($doc.machines).Count }
  } catch { }
}

$script:SkillDirs = @()
function Install-AgentSkill {
  if ($NoConfigure) { return }
  $roots = @()
  $claude = $env:CLAUDE_CONFIG_DIR; if (-not $claude) { $claude = Join-Path $env:USERPROFILE ".claude" }
  if ((Test-Path -LiteralPath $claude) -or (Get-Command claude -ErrorAction SilentlyContinue)) { $roots += (Join-Path $claude "skills") }
  $codex = Join-Path $env:USERPROFILE ".codex"
  if ((Test-Path -LiteralPath $codex) -or (Get-Command codex -ErrorAction SilentlyContinue)) { $roots += (Join-Path $codex "skills") }
  $skill = @'
---
name: franken-remote
description: List the user's Tailscale machines and open a FrankenRemote remote desktop window from Windows with fr-windows. Use when asked which machines are reachable or to open a view of another machine.
---

# FrankenRemote on Windows (`fr-windows`)

FrankenRemote is a remote desktop over Tailscale: Tailscale supplies
connectivity and identity, and each host decides who may view or control it.

```powershell
fr-windows hosts                              # JSON: machines on the tailnet (not proof they host)
fr-windows view NODE_ID                       # open a view-only window
fr-windows view NODE_ID --control           # request control (the host must allow it)
```

`NODE_ID` is a `stable_id` from `fr-windows hosts`. The desktop app
(`FrankenRemote` in the Start menu) offers the same choices in a window.

Rules:
- Only open machines the user named; view-only is the default.
- The viewer is a window for a person. Agent observation and input commands
  (`fr robot ...`) exist only in the macOS/Linux `fr` client.
- Never copy clipboard text or screen contents into logs or other tools
  unless asked.
'@
  foreach ($root in $roots) {
    $dest = Join-Path $root "franken-remote"
    $marker = Join-Path $dest ".franken_remote_installer"
    if ((Test-Path -LiteralPath $dest) -and -not (Test-Path -LiteralPath $marker)) { Write-Info "Keeping the existing $dest"; continue }
    New-Item -ItemType Directory -Force -Path $dest | Out-Null
    [System.IO.File]::WriteAllText((Join-Path $dest "SKILL.md"), $skill.Replace("`r`n", "`n"), (New-Object System.Text.UTF8Encoding $false))
    Set-Content -LiteralPath $marker -Value "" -NoNewline
    $script:SkillDirs += $dest
  }
  if ($script:SkillDirs.Count -gt 0) { Write-Ok "Agent skill installed: $($script:SkillDirs -join ', ')" }
}

function Invoke-Uninstall {
  $app = Join-Path $script:InstallDir "FrankenRemote.exe"
  if (-not (Test-Path -LiteralPath $app)) { throw "FrankenRemote is not installed in $($script:InstallDir)" }
  Stop-RunningCopies
  $process = Start-Process -FilePath $app -ArgumentList "--uninstall", "--quiet" -Wait -PassThru
  if ($process.ExitCode -ne 0) { throw "FrankenRemote --uninstall failed (exit $($process.ExitCode))" }
  foreach ($root in @((Join-Path $env:USERPROFILE ".claude\skills"), (Join-Path $env:USERPROFILE ".codex\skills"))) {
    $dest = Join-Path $root "franken-remote"
    if (Test-Path -LiteralPath (Join-Path $dest ".franken_remote_installer")) {
      Remove-Item -LiteralPath (Join-Path $dest "SKILL.md"), (Join-Path $dest ".franken_remote_installer") -Force -ErrorAction SilentlyContinue
      Remove-Item -LiteralPath $dest -ErrorAction SilentlyContinue
    }
  }
  if ((Test-Path -LiteralPath $script:CacheDir) -and (Read-YesNo "Remove the source build cache at $($script:CacheDir)?" $true)) {
    Remove-Item -LiteralPath $script:CacheDir -Recurse -Force
  }
  Write-Ok "FrankenRemote removed. Tailscale and the HEVC Video Extensions were not changed."
}

function Show-Summary {
  $lines = @(
    "FrankenRemote is installed",
    "",
    "Version:    $($script:InstalledVersion) ($($script:InstalledSource))",
    "Folder:     $($script:InstallDir)",
    "Start menu: FrankenRemote"
  )
  switch ($script:TailscaleState) {
    "running" {
      $count = ""; if ($null -ne $script:MachineCount) { $count = ", $($script:MachineCount) machines listed" }
      $lines += "Tailscale:  running$count"
    }
    "installed" { $lines += "Tailscale:  installed, not connected (sign in from its icon)" }
    default { $lines += "Tailscale:  NOT installed (https://tailscale.com/download/windows)" }
  }
  switch ($script:HevcState) {
    "installed" { $lines += "HEVC:       Microsoft HEVC Video Extensions" }
    "missing" { $lines += "HEVC:       not found; the app's status line confirms" }
  }
  if ($script:SkillDirs.Count -gt 0) { $lines += "Agent skill: $($script:SkillDirs.Count) installed" }
  $lines += ""
  $lines += "Next: open FrankenRemote from the Start menu, pick a machine, Connect."
  $lines += "      In a new terminal: fr-windows hosts"
  Write-Host ""
  Show-Box $lines "Green"
  Write-Host "To uninstall: Settings > Apps > Installed apps > FrankenRemote, or" -ForegroundColor DarkGray
  Write-Host "  & `"$($script:InstallDir)\FrankenRemote.exe`" --uninstall" -ForegroundColor DarkGray
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

function Invoke-Main {
  if ($Help) { Show-Usage; return }
  Show-Header
  Assert-Platform

  $mutex = New-Object System.Threading.Mutex($false, "Local\FrankenRemoteInstaller")
  if (-not $mutex.WaitOne(0)) { throw "Another FrankenRemote installer is running" }
  $temp = Join-Path ([IO.Path]::GetTempPath()) ("franken_remote-install-" + [Guid]::NewGuid().ToString("N"))
  New-Item -ItemType Directory -Path $temp | Out-Null
  try {
    if ($Uninstall) { Invoke-Uninstall; return }

    if ($Offline -and $FromSource) { throw "-Offline installs a release zip and cannot be combined with -FromSource or -Ref" }
    if ($Version) { $script:Tag = ConvertTo-Tag $Version }
    if (-not $Offline -and -not $ArtifactUrl -and -not $script:Tag -and -not $FromSource) {
      $script:Tag = Resolve-LatestTag
      if ($script:Tag) { Write-Info "Latest release: $($script:Tag)" }
      else {
        Write-Warn "No FrankenRemote release has been published yet."
        $FromSource = $true
      }
    }

    Test-Tailscale
    Test-HevcDecoder

    $payload = $null
    if (-not $FromSource) { $payload = Get-ReleasePayload $temp }
    if (-not $payload) {
      if ($Offline) { throw "The offline zip could not be installed" }
      $payload = Build-FromSource $temp
    }

    if (-not $Force -and (Test-SameBuild $payload)) {
      Write-Ok "This build is already installed (use -Force to reinstall)"
    } else {
      Install-Payload $payload
    }
    Test-InstalledCli
    Install-AgentSkill
    Show-Summary
    if (-not $NoLaunch) {
      Start-Process -FilePath (Join-Path $script:InstallDir "FrankenRemote.exe")
    }
  } finally {
    Remove-Item -LiteralPath $temp -Recurse -Force -ErrorAction SilentlyContinue
    $mutex.ReleaseMutex()
    $mutex.Dispose()
  }
}

$script:Tag = $null
$script:Commit = ""
$script:InstalledVersion = ""
$script:InstalledSource = ""
# FR_INSTALL_LIB_ONLY=1 loads the functions without installing (tests).
if (Test-EnvFlag "FR_INSTALL_LIB_ONLY") { return }
try {
  Invoke-Main
} catch {
  Write-Err $_.Exception.Message
  # `exit` would close the window of an `irm | iex` session; only a script
  # file run reports failure through its exit code.
  if ($MyInvocation.MyCommand.Path) { exit 1 }
}
