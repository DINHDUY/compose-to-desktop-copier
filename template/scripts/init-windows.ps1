# Install the Windows toolchain used by `make dev`.
# Run from Git Bash or MSYS2 via `make init`.
# Docker Desktop is never installed. A working Docker engine is kept.
# Podman is installed when Docker Desktop is absent.

$ErrorActionPreference = "Stop"

$Root = Split-Path $PSScriptRoot -Parent
Set-Location $Root

$MinRust = "1.77.2"
$MinCompose = "2.20.0"
$CargoToml = Join-Path $Root "src-tauri\Cargo.toml"
if (Test-Path $CargoToml) {
    $match = Select-String -Path $CargoToml -Pattern '^rust-version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if ($match) {
        $MinRust = $match.Matches[0].Groups[1].Value
    }
}

$Failures = New-Object System.Collections.Generic.List[string]
$Engine = "unknown"

function Add-Failure([string]$Message) {
    $Failures.Add($Message) | Out-Null
    Write-Host "init: $Message" -ForegroundColor Yellow
}

function Test-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Update-ProcessPath {
    $machine = [Environment]::GetEnvironmentVariable("Path", "Machine")
    $user = [Environment]::GetEnvironmentVariable("Path", "User")
    $env:Path = "$user;$machine"
    $cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
    if (Test-Path $cargoBin) {
        $env:Path = "$cargoBin;$env:Path"
    }
    $localBin = Join-Path $env:USERPROFILE ".local\bin"
    if (Test-Path $localBin) {
        $env:Path = "$localBin;$env:Path"
    }
}

function Test-VersionAtLeast([string]$Actual, [string]$Minimum) {
    if (-not $Actual) { return $false }
    try {
        $left = ($Actual.Trim().TrimStart("v") -split "[^0-9\.]")[0]
        $right = ($Minimum.Trim().TrimStart("v") -split "[^0-9\.]")[0]
        return ([version]$left) -ge ([version]$right)
    } catch {
        return $false
    }
}

function Test-WingetId([string]$Id) {
    if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
        return $false
    }
    winget list --id $Id -e --disable-interactivity --accept-source-agreements | Out-Null
    return $LASTEXITCODE -eq 0
}

function Install-WingetPackage([string]$Id, [string]$Override, [switch]$Admin) {
    if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
        Add-Failure "winget is missing. Install App Installer, then rerun make init."
        return
    }
    if (Test-WingetId $Id) {
        Write-Host "Already installed: $Id"
        return
    }
    if ($Admin -and -not (Test-Administrator)) {
        $command = "winget install --id $Id -e --accept-package-agreements --accept-source-agreements"
        if ($Override) {
            $command += " --override `"$Override`""
        }
        Add-Failure "An elevated PowerShell is required to install $Id. Run: $command"
        return
    }
    $arguments = @(
        "install", "--id", $Id, "-e",
        "--accept-package-agreements", "--accept-source-agreements",
        "--disable-interactivity"
    )
    if ($Override) {
        $arguments += @("--override", $Override)
    }
    & winget @arguments
    if ($LASTEXITCODE -ne 0) {
        Add-Failure "winget install $Id failed with exit code $LASTEXITCODE."
    }
    Update-ProcessPath
}

function Test-MsvcBuildTools {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) {
        return $false
    }
    $install = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    return -not [string]::IsNullOrWhiteSpace($install)
}

function Test-WebView2 {
    $keys = @(
        "HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}",
        "HKLM:\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}",
        "HKCU:\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
    )
    foreach ($key in $keys) {
        $version = (Get-ItemProperty -Path $key -ErrorAction SilentlyContinue).pv
        if ($version -and $version -ne "0.0.0.0") {
            return $true
        }
    }
    return $false
}

function Test-DockerDesktopInstalled {
    $candidates = @(
        (Join-Path $env:ProgramFiles "Docker\Docker\Docker Desktop.exe"),
        (Join-Path ${env:ProgramFiles(x86)} "Docker\Docker\Docker Desktop.exe")
    )
    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path $candidate)) {
            return $true
        }
    }
    return $false
}

function Write-Runtime([string]$Value) {
    $path = Join-Path $Root ".container-runtime"
    $utf8 = New-Object System.Text.UTF8Encoding $false
    [System.IO.File]::WriteAllText($path, "$Value`n", $utf8)
    Write-Host "Wrote .container-runtime ($Value)"
    $script:Engine = $Value
}

function Install-RustToolchain {
    Update-ProcessPath
    $rustc = Get-Command rustc -ErrorAction SilentlyContinue
    $needsInstall = -not $rustc
    if ($rustc) {
        $version = (& rustc --version).Split(" ")[1]
        $needsInstall = -not (Test-VersionAtLeast $version $MinRust)
    }
    if ($needsInstall) {
        Install-WingetPackage "Rustup.Rustup" ""
        Update-ProcessPath
        if (Get-Command rustup -ErrorAction SilentlyContinue) {
            & rustup toolchain install stable
            & rustup default stable
        }
    }
    Update-ProcessPath
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Add-Failure "cargo is not on PATH. Open a new shell and rerun make init."
        return
    }
    $version = (& rustc --version).Split(" ")[1]
    if (-not (Test-VersionAtLeast $version $MinRust)) {
        Add-Failure "rustc $version is older than $MinRust."
    }
}

function Install-TauriCli {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        return
    }
    $major = $null
    try {
        $versionLine = & cargo tauri --version 2>$null
        if ($LASTEXITCODE -eq 0 -and $versionLine) {
            $major = ($versionLine -split "\s+")[1].Split(".")[0]
        }
    } catch {
        $major = $null
    }
    if ($major -ne "2") {
        Write-Host "Installing tauri-cli ^2"
        & cargo install tauri-cli --version "^2" --force
        if ($LASTEXITCODE -ne 0) {
            Add-Failure "cargo install tauri-cli failed."
        }
    }
}

function Test-WslInstalled {
    if (-not (Get-Command wsl -ErrorAction SilentlyContinue)) {
        return $false
    }
    & wsl --status *> $null
    return $LASTEXITCODE -eq 0
}

function Start-PodmanMachine {
    & podman machine inspect podman-machine-default *> $null
    if ($LASTEXITCODE -ne 0) {
        Write-Host "Initializing the Podman machine"
        & podman machine init
        if ($LASTEXITCODE -ne 0) {
            Add-Failure "podman machine init failed."
            return
        }
    }
    & podman info *> $null
    if ($LASTEXITCODE -eq 0) {
        return
    }
    Write-Host "Starting the Podman machine"
    & podman machine start
    if ($LASTEXITCODE -ne 0) {
        & podman info *> $null
        if ($LASTEXITCODE -ne 0) {
            Add-Failure "Podman is installed and the machine did not start."
        }
    }
}

function Install-PodmanEngine {
    Install-WingetPackage "RedHat.Podman" "" -Admin
    Update-ProcessPath
    if (-not (Get-Command podman -ErrorAction SilentlyContinue)) {
        Add-Failure "podman is not on PATH."
        return
    }
    if (-not (Test-WslInstalled)) {
        Add-Failure "WSL is missing. From an administrator PowerShell run: wsl --install --no-distribution. Then restart and rerun make init."
        return
    }
    Write-Host "Downloading the pinned Compose provider"
    try {
        & (Join-Path $PSScriptRoot "fetch-runtime.ps1")
    } catch {
        Add-Failure "fetch-runtime failed to download the Compose provider. $($_.Exception.Message)"
        return
    }
    Start-PodmanMachine
}

function Select-Engine {
    $docker = Get-Command docker -ErrorAction SilentlyContinue
    if ($docker) {
        & docker info *> $null
        if ($LASTEXITCODE -eq 0) {
            Write-Runtime "docker"
            return
        }
    }
    if (Test-DockerDesktopInstalled) {
        Write-Runtime "docker"
        Add-Failure "Docker Desktop is installed and the daemon is stopped. Start Docker Desktop, then rerun make dev."
        return
    }
    if ($docker) {
        $info = & docker info 2>&1 | Out-String
        if ($info -match "permission denied") {
            Write-Runtime "docker"
            Add-Failure "Docker is installed and this user cannot access the daemon. Open a new shell, then rerun make dev."
            return
        }
    }
    Write-Host "Docker is not available. Installing Podman."
    Install-PodmanEngine
    Write-Runtime "podman"
}

function Show-ReportLine([string]$Label, [scriptblock]$Command) {
    try {
        $output = & $Command 2>$null
        if ($LASTEXITCODE -and $LASTEXITCODE -ne 0) {
            throw "exit $LASTEXITCODE"
        }
        $text = ($output | Select-Object -First 1)
        if (-not $text) { throw "empty" }
        Write-Host ("  {0,-14} {1}" -f $Label, $text)
        return $true
    } catch {
        Write-Host ("  {0,-14} missing" -f $Label)
        return $false
    }
}

Update-ProcessPath

if (-not (Test-MsvcBuildTools)) {
    Install-WingetPackage "Microsoft.VisualStudio.2022.BuildTools" "--wait --passive --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended" -Admin
}
if (-not (Test-WebView2)) {
    Install-WingetPackage "Microsoft.EdgeWebView2Runtime" "" -Admin
}

Install-RustToolchain
Install-TauriCli
Select-Engine

Write-Host ""
Write-Host "Development environment"
Write-Host ("  {0,-14} {1}" -f "engine", $Engine)
if (-not (Show-ReportLine "rustc" { rustc --version })) {
    Add-Failure "rustc $MinRust or newer is required."
}
if (-not (Show-ReportLine "cargo tauri" { cargo tauri --version })) {
    Add-Failure "tauri-cli ^2 is required."
}
if (-not (Test-MsvcBuildTools)) {
    Add-Failure "MSVC Build Tools with the C++ x64 workload are required."
}
if (-not (Test-WebView2)) {
    Add-Failure "The WebView2 runtime is required."
}

if ($Engine -eq "docker") {
    Show-ReportLine "docker" { docker --version } | Out-Null
    & docker info *> $null
    if ($LASTEXITCODE -eq 0) {
        $composeVersion = & docker compose version --short 2>$null
        if ($composeVersion) {
            Write-Host ("  {0,-14} {1}" -f "compose", $composeVersion)
            if (-not (Test-VersionAtLeast $composeVersion $MinCompose)) {
                Add-Failure "Docker Compose $composeVersion is older than $MinCompose."
            }
        } else {
            Add-Failure "docker compose is required."
        }
    }
} elseif ($Engine -eq "podman") {
    if (-not (Show-ReportLine "podman" { podman --version })) {
        Add-Failure "podman is required."
    }
    & podman info *> $null
    if ($LASTEXITCODE -ne 0) {
        Add-Failure "Podman is installed and the engine is not running."
    }
    $compose = Join-Path $Root "installer\cache\docker-compose.exe"
    if ((Test-Path $compose) -and ((Get-Item $compose).Length -ge 1MB)) {
        $composeVersion = & $compose version --short 2>$null
        Write-Host ("  {0,-14} {1} ({2})" -f "compose", $composeVersion, $compose)
        if (-not (Test-VersionAtLeast "$composeVersion" $MinCompose)) {
            Add-Failure "Compose $composeVersion is older than $MinCompose."
        }
    } else {
        Add-Failure "installer\cache\docker-compose.exe is missing. Rerun make init."
    }
}

$envFile = Join-Path $Root ".env"
if (Test-Path $envFile) {
    Write-Host ("  {0,-14} {1}" -f ".env", ".env")
}

Write-Host ""
if ($Failures.Count -gt 0) {
    Write-Host "Still missing:"
    foreach ($item in $Failures) {
        Write-Host "  - $item"
    }
    exit 1
}

Write-Host "Ready for make dev."
