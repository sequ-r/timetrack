<#
.SYNOPSIS
  Build the TimeTrack Windows installer (Inno Setup, #158).

.DESCRIPTION
  Compiles the four release binaries, stages them next to timetrack.iss,
  optionally Authenticode-signs them, compiles the installer with ISCC, signs
  the installer too, and copies it to -OutputDir as
  TimeTrack-Setup-<version>.exe.

  Signing needs a maintainer-supplied PFX in the environment (see README.md
  next to this script). Without it the build still succeeds but prints an
  UNSIGNED warning: the artifacts work, they just show a SmartScreen
  "unknown publisher" prompt. That unsigned fallback is deliberate -- see
  the release notes, which must say so explicitly.

.EXAMPLE
  .\build-installer.ps1 -Version 0.2.0
#>
[CmdletBinding()]
param(
  # Release version (X.Y.Z). Defaults to the workspace Cargo.toml version,
  # which release.yml has already minted before calling this.
  [string]$Version = '',
  [string]$Configuration = 'Release',
  # Skip `cargo build` and package whatever is already in target\<Configuration>.
  [switch]$SkipBuild,
  # Where the finished TimeTrack-Setup-<version>.exe lands.
  [string]$OutputDir = ''
)

$ErrorActionPreference = 'Stop'

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = (Resolve-Path (Join-Path $ScriptDir '..\..')).Path
$StageDir = Join-Path $ScriptDir 'bin'
$IssFile = Join-Path $ScriptDir 'timetrack.iss'

$Binaries = @('timetrack-gui.exe', 'timetrack-service.exe', 'timetrack.exe', 'timetrack-tray.exe')

function Get-WorkspaceVersion {
  $line = Select-String -Path (Join-Path $RepoRoot 'Cargo.toml') `
    -Pattern '^version = "(\d+\.\d+\.\d+)"$' | Select-Object -First 1
  if ($null -eq $line) { throw 'no workspace version line in Cargo.toml' }
  return $line.Matches[0].Groups[1].Value
}

function Find-Iscc {
  $cmd = Get-Command 'iscc' -ErrorAction SilentlyContinue
  if ($cmd) { return $cmd.Source }
  foreach ($p in @(
      "$env:ProgramFiles\Inno Setup 7\ISCC.exe",
      "${env:ProgramFiles(x86)}\Inno Setup 7\ISCC.exe",
      "$env:LOCALAPPDATA\Programs\Inno Setup 7\ISCC.exe",
      "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
      "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
      "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe")) {
    if (Test-Path $p) { return $p }
  }
  throw ('ISCC.exe not found. Install Inno Setup 7 (release.yml pins ' +
    'innosetup-7.1.1-x64.exe with a checksum) or put ISCC.exe on PATH.')
}

function Find-Signtool {
  $cmd = Get-Command 'signtool' -ErrorAction SilentlyContinue
  if ($cmd) { return $cmd.Source }
  $sdk = Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\bin' `
    -Directory -ErrorAction SilentlyContinue |
    Sort-Object Name -Descending | Select-Object -First 1
  if ($sdk) {
    $p = Join-Path $sdk.FullName 'x64\signtool.exe'
    if (Test-Path $p) { return $p }
  }
  return $null
}

if ($Version -eq '') { $Version = Get-WorkspaceVersion }
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "not a version: $Version (need X.Y.Z)" }
if ($OutputDir -eq '') { $OutputDir = Join-Path $RepoRoot 'dist' }
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null

Write-Host "TimeTrack Windows installer, version $Version"

if (-not $SkipBuild) {
  Write-Host '-- cargo build --release'
  & cargo build --release --locked --manifest-path (Join-Path $RepoRoot 'Cargo.toml') `
    --package timetrack-gui --package timetrack-service `
    --package timetrack-cli --package timetrack-tray
  if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit $LASTEXITCODE" }
}

$targetDir = Join-Path $RepoRoot "target\$Configuration"
New-Item -ItemType Directory -Force -Path $StageDir | Out-Null
foreach ($b in $Binaries) {
  $src = Join-Path $targetDir $b
  if (-not (Test-Path $src)) {
    throw "missing binary: $src (build with -Configuration $Configuration first)"
  }
  Copy-Item $src (Join-Path $StageDir $b) -Force
}

# Signing, when the maintainer secrets are present. The PFX arrives base64'd
# so it can live in a GitHub secret; the password may legitimately be empty.
$signing = $false
$pfxPath = $null
$signtool = $null
if (-not [string]::IsNullOrEmpty($env:WINDOWS_PFX_BASE64)) {
  $signtool = Find-Signtool
  if ($null -eq $signtool) {
    throw 'WINDOWS_PFX_BASE64 is set but no signtool.exe was found (needs the Windows 10/11 SDK)'
  }
  $pfxPath = Join-Path ([System.IO.Path]::GetTempPath()) 'timetrack-sign.pfx'
  [System.IO.File]::WriteAllBytes($pfxPath,
    [System.Convert]::FromBase64String($env:WINDOWS_PFX_BASE64))
  $signing = $true
}

function Invoke-Signtool([string[]]$Files) {
  $signArgs = @('sign', '/fd', 'SHA256', '/tr', 'http://timestamp.digicert.com',
    '/td', 'SHA256', '/f', $pfxPath)
  if (-not [string]::IsNullOrEmpty($env:WINDOWS_PFX_PASSWORD)) {
    $signArgs += @('/p', $env:WINDOWS_PFX_PASSWORD)
  }
  $signArgs += $Files
  & $signtool @signArgs
  if ($LASTEXITCODE -ne 0) { throw "signtool failed with exit $LASTEXITCODE" }
}

try {
  if ($signing) {
    Write-Host '-- signing staged binaries'
    Invoke-Signtool ($Binaries | ForEach-Object { Join-Path $StageDir $_ })
  } else {
    Write-Warning ('UNSIGNED build: no WINDOWS_PFX_BASE64 in the environment. ' +
      'The installer works but SmartScreen will warn about an unknown publisher. ' +
      'Releases must say so explicitly in their notes.')
  }

  Write-Host '-- ISCC timetrack.iss'
  $iscc = Find-Iscc
  & $iscc "/DMyAppVersion=$Version" $IssFile
  if ($LASTEXITCODE -ne 0) { throw "ISCC failed with exit $LASTEXITCODE" }

  $built = Join-Path $ScriptDir "Output\TimeTrack-Setup-$Version.exe"
  if (-not (Test-Path $built)) { throw "ISCC produced no $built" }

  if ($signing) {
    Write-Host '-- signing installer'
    Invoke-Signtool @($built)
    Write-Host "signed: $built"
  }

  $dest = Join-Path $OutputDir "TimeTrack-Setup-$Version.exe"
  Copy-Item $built $dest -Force
  $sha = (Get-FileHash $dest -Algorithm SHA256).Hash.ToLower()
  Write-Host "installer: $dest"
  Write-Host "sha256:    $sha"
} finally {
  if (($null -ne $pfxPath) -and (Test-Path $pfxPath)) { Remove-Item $pfxPath -Force }
}
