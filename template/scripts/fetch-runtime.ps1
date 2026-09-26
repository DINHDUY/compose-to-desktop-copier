# Downloads the pinned Podman MSI and Compose provider into installer/cache.
$ErrorActionPreference = "Stop"

$root = Split-Path $PSScriptRoot -Parent
$versionsPath = Join-Path $root "installer\versions.json"
$cache = Join-Path $root "installer\cache"
New-Item -ItemType Directory -Force -Path $cache | Out-Null

$versions = Get-Content -Raw -Path $versionsPath | ConvertFrom-Json

function Get-UpstreamFile([string]$Url, [string]$Destination) {
    Write-Host "Downloading $Url"
    Invoke-WebRequest -Uri $Url -OutFile $Destination -UseBasicParsing
}

function Get-ExpectedHash([string]$ChecksumFile, [string]$FileName) {
    $line = Get-Content -Path $ChecksumFile | Where-Object { $_ -like "*$FileName*" } | Select-Object -First 1
    if (-not $line) {
        throw "No checksum for $FileName in $ChecksumFile"
    }
    if ($line -match "([A-Fa-f0-9]{64})") {
        return $Matches[1].ToLowerInvariant()
    }
    throw "Could not parse checksum for $FileName"
}

function Save-VerifiedFile(
    [string]$Url,
    [string]$ChecksumUrl,
    [string]$UpstreamName,
    [string]$Destination,
    [string]$PinnedSha256
) {
    if (-not $PinnedSha256) {
        throw "versions.json is missing the pinned sha256 for $UpstreamName"
    }
    $pinned = $PinnedSha256.ToLowerInvariant()
    $checksumFile = Join-Path $env:TEMP ("compose-shell-" + [IO.Path]::GetFileName($ChecksumUrl))
    Get-UpstreamFile $ChecksumUrl $checksumFile
    $expected = Get-ExpectedHash $checksumFile $UpstreamName
    if ($expected -ne $pinned) {
        throw "Upstream checksum for $UpstreamName does not match the pinned sha256"
    }
    if (Test-Path $Destination) {
        $current = (Get-FileHash -Algorithm SHA256 -Path $Destination).Hash.ToLowerInvariant()
        if ($current -eq $pinned) {
            Write-Host "Already fetched $UpstreamName"
            return
        }
    }
    $partial = "$Destination.partial"
    Get-UpstreamFile $Url $partial
    $actual = (Get-FileHash -Algorithm SHA256 -Path $partial).Hash.ToLowerInvariant()
    if ($actual -ne $pinned) {
        Remove-Item -Force $partial
        throw "Checksum mismatch for $UpstreamName"
    }
    Move-Item -Force $partial $Destination
}

function Save-License([string]$Url, [string]$Destination) {
    Get-UpstreamFile $Url $Destination
    if ((Get-Item $Destination).Length -lt 32) {
        throw "License download was empty: $Url"
    }
}

$podmanMsi = Join-Path $cache "podman-installer.msi"
$composeExe = Join-Path $cache "docker-compose.exe"

$composePin = [string]$versions.compose.sha256.($versions.compose.fileName)
Save-VerifiedFile $versions.podman.msiUrl $versions.podman.checksumUrl $versions.podman.fileName $podmanMsi $versions.podman.sha256
Save-VerifiedFile $versions.compose.binaryUrl $versions.compose.checksumUrl $versions.compose.fileName $composeExe $composePin
Save-License $versions.podman.licenseUrl (Join-Path $cache "LICENSE-podman")
Save-License $versions.compose.licenseUrl (Join-Path $cache "LICENSE-compose")

foreach ($path in @($podmanMsi, $composeExe)) {
    if ((Get-Item $path).Length -lt 1MB) {
        throw "$path is too small to be the real runtime binary"
    }
}

Write-Host "Podman $($versions.podman.version) and Compose $($versions.compose.version) are in installer\cache"
