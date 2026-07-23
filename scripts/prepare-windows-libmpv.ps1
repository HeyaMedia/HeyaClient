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

$dumpbin = Get-Command "dumpbin.exe" -ErrorAction SilentlyContinue
if ($null -ne $dumpbin) {
    $dependencyNames = @(
        & $dumpbin.Source /dependents $dll |
            Select-String -Pattern '^\s+([A-Za-z0-9_.-]+\.dll)\s*$' |
            ForEach-Object { $_.Matches[0].Groups[1].Value } |
            Sort-Object -Unique
    )
    foreach ($dependency in $dependencyNames) {
        $runtimeDependency = Join-Path $extractDirectory $dependency
        $systemDependency = Join-Path "$env:SystemRoot/System32" $dependency
        $availability = if (Test-Path $runtimeDependency -PathType Leaf) {
            "provider"
        } elseif (Test-Path $systemDependency -PathType Leaf) {
            "system"
        } elseif ($dependency.StartsWith("api-ms-", [StringComparison]::OrdinalIgnoreCase)) {
            "Windows API set"
        } else {
            "MISSING"
        }
        Write-Host "libmpv dependency: $dependency ($availability)"
    }
}

if ($null -eq ("HeyaNativeLibrary" -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class HeyaNativeLibrary {
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr LoadLibraryEx(string path, IntPtr file, uint flags);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool FreeLibrary(IntPtr module);
}
'@
}

$libraryHandle = [HeyaNativeLibrary]::LoadLibraryEx($dll, [IntPtr]::Zero, 0x00000100 -bor 0x00000800)
if ($libraryHandle -eq [IntPtr]::Zero) {
    $loadError = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
    $loadMessage = [ComponentModel.Win32Exception]::new($loadError).Message
    Write-Warning "The verified provider library is not loadable on this Windows image (Win32 error ${loadError}: ${loadMessage}). Compile-time validation can continue, but runtime adapter tests require Desktop/App Compatibility components."
} else {
    [void][HeyaNativeLibrary]::FreeLibrary($libraryHandle)
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
