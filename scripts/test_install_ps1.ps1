param([Parameter(Mandatory=$true)][string]$Binary)
$ErrorActionPreference = 'Stop'
$Binary = (Resolve-Path $Binary).Path
$installer = Join-Path (Split-Path $PSScriptRoot -Parent) 'install.ps1'
$root = Join-Path ([IO.Path]::GetTempPath()) ('zhandu-installer-' + [guid]::NewGuid())
$originalDir = $env:ZHANDU_INSTALL_DIR
$originalVersion = $env:ZHANDU_VERSION
$version = 'v' + ((& $Binary --version) -replace '^zhandu ', '')
$script:fixtureVersion = $version
$folder = "zhandu-$version-x86_64-pc-windows-msvc"
$archive = "$folder.zip"
New-Item -ItemType Directory -Path (Join-Path $root $folder) -Force | Out-Null
Copy-Item $Binary (Join-Path (Join-Path $root $folder) 'zhandu.exe')
Compress-Archive -Path (Join-Path $root $folder) -DestinationPath (Join-Path $root $archive)
$manifest = Join-Path $root 'SHA256SUMS'
$hash = (Get-FileHash (Join-Path $root $archive) -Algorithm SHA256).Hash
Set-Content $manifest "$hash  $archive"
# Replace only the download transport; exercise the actual installer/executable.
function Invoke-WebRequest {
    param([string]$Uri, [string]$OutFile, [switch]$UseBasicParsing)
    if (-not $Uri.StartsWith('https://github.com/zhanxw/zhandu/releases/')) { throw 'Unexpected URL' }
    if ($Uri.EndsWith('/latest')) {
        return [pscustomobject]@{BaseResponse=[pscustomobject]@{ResponseUri=[uri]"https://github.com/zhanxw/zhandu/releases/tag/$script:fixtureVersion"}}
    }
    if (-not $Uri.Contains("/download/$script:fixtureVersion/")) { throw 'Missing release' }
    Copy-Item (Join-Path $root ($Uri.Split('/')[-1])) $OutFile
}
function Assert-Installed {
    if ((& (Join-Path $env:ZHANDU_INSTALL_DIR 'zhandu.exe') --version) -ne "zhandu $($version.Substring(1))") { throw 'Wrong installed version' }
}
function Expect-Failure {
    $failed = $false
    try { & $installer } catch { $failed = $true }
    if (-not $failed) { throw 'Expected installer to reject this input' }
}
try {
    $env:ZHANDU_INSTALL_DIR = Join-Path $root 'install with spaces'
    $env:ZHANDU_VERSION = $version
    & $installer
    Assert-Installed
    # An existing executable is replaced only after verification.
    $destination = Join-Path $env:ZHANDU_INSTALL_DIR 'zhandu.exe'
    Set-Content $destination 'old installation'
    $env:ZHANDU_VERSION = $null
    & $installer
    Assert-Installed
    & $installer
    Assert-Installed
    $before = (Get-FileHash $destination).Hash
    $env:ZHANDU_VERSION = '../invalid'
    Expect-Failure
    $env:ZHANDU_VERSION = 'v99.99.99'
    Expect-Failure
    $env:ZHANDU_VERSION = $null
    Set-Content $manifest (('0' * 64) + "  $archive")
    Expect-Failure
    Set-Content $manifest ''
    Expect-Failure
    if ((Get-FileHash $destination).Hash -ne $before) { throw 'Failed update damaged the installed binary' }
    if (@(Get-ChildItem $env:ZHANDU_INSTALL_DIR).Count -ne 1) { throw 'Staged files were not cleaned up' }
    Write-Host 'PASS: install, upgrade, reinstall, pinning, invalid version, missing downloads, checksum rejection, preservation, cleanup'
} finally {
    $env:ZHANDU_INSTALL_DIR = $originalDir
    $env:ZHANDU_VERSION = $originalVersion
    Remove-Item $root -Recurse -Force
}
