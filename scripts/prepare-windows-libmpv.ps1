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

$receipt = [ordered]@{
    schemaVersion = 1
    provider = [string]$manifest.windows.provider
    providerPage = [string]$manifest.windows.providerPage
    release = [string]$manifest.windows.release
    mpvRevision = [string]$manifest.windows.mpvRevision
    architecture = $Architecture
    archiveSha256 = $actualHash
    library = "libmpv-2.dll"
    ciTestRuntimeOnly = $true
}
$receipt | ConvertTo-Json | Set-Content -Encoding UTF8 (Join-Path $extractDirectory "provider-receipt.json")

Write-Output $extractDirectory
