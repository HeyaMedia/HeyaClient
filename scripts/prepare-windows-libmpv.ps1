[CmdletBinding()]
param(
    [ValidateSet("x86_64", "aarch64")]
    [string]$Architecture = "x86_64",

    [Parameter(Mandatory = $true)]
    [string]$OutputDirectory
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$repoRoot = Split-Path -Parent $PSScriptRoot
$manifestPath = Join-Path $repoRoot "src-tauri/native/mpv/providers-v1.json"
$manifest = Get-Content -Raw $manifestPath | ConvertFrom-Json
$assetProperty = $manifest.windows.assets.PSObject.Properties[$Architecture]
if ($null -eq $assetProperty) {
    throw "The MPV provider manifest has no Windows asset for $Architecture."
}
$asset = $assetProperty.Value
$uri = [Uri]$asset.url
if ($uri.Scheme -ne "https" -or $uri.Host -ne "github.com" -or
    -not $uri.AbsolutePath.StartsWith("/shinchiro/mpv-winbuild-cmake/releases/download/")) {
    throw "The pinned Windows MPV provider URL is outside the approved GitHub release path."
}

$output = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Force $output | Out-Null
$archive = Join-Path $output "provider.7z"
Invoke-WebRequest -Uri $uri.AbsoluteUri -OutFile $archive -MaximumRedirection 5

$archiveInfo = Get-Item $archive
if ($archiveInfo.Length -gt [int64]$manifest.windows.maximumDownloadBytes) {
    throw "The Windows MPV provider archive exceeds its declared maximum size."
}
$actualHash = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
$expectedHash = [string]$asset.sha256
if ($actualHash -ne $expectedHash.ToLowerInvariant()) {
    throw "The Windows MPV provider archive failed SHA-256 verification."
}

$sevenZip = Get-Command "7z.exe" -ErrorAction SilentlyContinue
if ($null -eq $sevenZip) {
    $fallback = Join-Path $env:ProgramFiles "7-Zip/7z.exe"
    if (-not (Test-Path $fallback -PathType Leaf)) {
        throw "7-Zip is required to prepare the pinned Windows MPV development runtime."
    }
    $sevenZipPath = $fallback
} else {
    $sevenZipPath = $sevenZip.Source
}

$extractDirectory = Join-Path $output "runtime"
New-Item -ItemType Directory -Force $extractDirectory | Out-Null
& $sevenZipPath x $archive "-o$extractDirectory" -y | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "7-Zip could not extract the Windows MPV provider archive."
}

$dll = Join-Path $extractDirectory "libmpv-2.dll"
if (-not (Test-Path $dll -PathType Leaf)) {
    throw "The verified provider archive did not contain libmpv-2.dll."
}

$programFilesX86 = [Environment]::GetFolderPath("ProgramFilesX86")
$vswhere = Join-Path $programFilesX86 "Microsoft Visual Studio/Installer/vswhere.exe"
if (-not (Test-Path $vswhere -PathType Leaf)) {
    throw "Microsoft Visual Studio Build Tools could not be located."
}
$requiredToolComponent = if ($Architecture -eq "aarch64") {
    "Microsoft.VisualStudio.Component.VC.Tools.ARM64"
} else {
    "Microsoft.VisualStudio.Component.VC.Tools.x86.x64"
}
$installationPath = & $vswhere -latest -products * `
    -requires $requiredToolComponent `
    -property installationPath
if (-not $installationPath) {
    throw "The Visual C++ tools for $Architecture are required for the Windows MPV preview."
}
$toolsRoot = Join-Path $installationPath "VC/Tools/MSVC"
$toolsVersion = Get-ChildItem $toolsRoot -Directory |
    Sort-Object { [Version]$_.Name } -Descending |
    Select-Object -First 1
if ($null -eq $toolsVersion) {
    throw "The installed Visual C++ toolchain has no MSVC tools directory."
}
$targetDirectory = if ($Architecture -eq "aarch64") { "arm64" } else { "x64" }
$machine = if ($Architecture -eq "aarch64") { "ARM64" } else { "X64" }
$toolDirectory = Join-Path $toolsVersion.FullName "bin/Hostx64/$targetDirectory"
$dumpbin = Join-Path $toolDirectory "dumpbin.exe"
$libTool = Join-Path $toolDirectory "lib.exe"
if (-not (Test-Path $dumpbin -PathType Leaf) -or -not (Test-Path $libTool -PathType Leaf)) {
    throw "The required MSVC import-library tools could not be located."
}

$dump = & $dumpbin /nologo /exports $dll
if ($LASTEXITCODE -ne 0) {
    throw "dumpbin could not inspect libmpv-2.dll."
}
$exports = foreach ($line in $dump) {
    if ($line -match '^\s+\d+\s+[0-9A-Fa-f]+\s+[0-9A-Fa-f]+\s+([A-Za-z_][A-Za-z0-9_@?$]*)\s*$') {
        $Matches[1]
    }
}
$exports = @($exports | Sort-Object -Unique)
if ($exports.Count -lt 20 -or -not ($exports -contains "mpv_create")) {
    throw "The provider DLL did not expose the expected libmpv API."
}

$definition = Join-Path $extractDirectory "mpv.def"
@('LIBRARY "libmpv-2.dll"', "EXPORTS") + $exports | Set-Content -Encoding Ascii $definition
$importLibrary = Join-Path $extractDirectory "mpv.lib"
& $libTool /nologo "/def:$definition" "/machine:$machine" "/out:$importLibrary" | Out-Null
if ($LASTEXITCODE -ne 0 -or -not (Test-Path $importLibrary -PathType Leaf)) {
    throw "MSVC could not generate the libmpv import library."
}

$receipt = [ordered]@{
    schemaVersion = 1
    provider = [string]$manifest.windows.provider
    providerPage = [string]$manifest.windows.providerPage
    release = [string]$manifest.windows.release
    mpvRevision = [string]$manifest.windows.mpvRevision
    architecture = $Architecture
    archiveSha256 = $actualHash
    library = "libmpv-2.dll"
    developmentPreviewOnly = $true
}
$receipt | ConvertTo-Json | Set-Content -Encoding UTF8 (Join-Path $extractDirectory "provider-receipt.json")
Remove-Item $archive -Force

Write-Output $extractDirectory
