#Requires -Version 5.1

<#
.SYNOPSIS
Runs the real S3 and Windows two-device sync tests against an isolated local RustFS.
.DESCRIPTION
Requires Docker Desktop using Linux containers and Windows Cloud Files support.
Generates local-only credentials in the ignored .local/rustfs.env on first use.
Uses only loopback ports 19000/19001 and the bucket bucket-mount-test. Each test
uses its own object prefix. The test container stops afterwards unless KeepRunning
is supplied; its named data and logs volumes are retained.
.PARAMETER TestExecutable
An already-built tests/s3_live.rs test executable. Skips Cargo and compiler setup.
.PARAMETER GnuToolchainBin
LLVM MinGW or MinGW bin directory. Otherwise, uses Visual Studio C++ build tools.
.EXAMPLE
.\scripts\test-rustfs.ps1 -GnuToolchainBin 'C:\tools\llvm-mingw\bin' -Offline
.EXAMPLE
.\scripts\test-rustfs.ps1 -TestExecutable '.\target\debug\deps\s3_live-HASH.exe'
.EXAMPLE
.\scripts\test-rustfs.ps1 -KeepRunning
#>
[CmdletBinding()]
param(
    [string]$GnuToolchainBin,
    [string]$TestExecutable,
    [switch]$Offline,
    [switch]$KeepRunning
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'These tests require Windows with Cloud Files support.'
}

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$composeFile = Join-Path $repositoryRoot 'compose.rustfs.yml'
$localDirectory = Join-Path $repositoryRoot '.local'
$credentialFile = Join-Path $localDirectory 'rustfs.env'
if ($TestExecutable) {
    $TestExecutable = (Resolve-Path -LiteralPath $TestExecutable).ProviderPath
    if (-not (Test-Path -LiteralPath $TestExecutable -PathType Leaf)) {
        throw '-TestExecutable must name an existing test executable.'
    }
}
if ($GnuToolchainBin) {
    $GnuToolchainBin = (Resolve-Path -LiteralPath $GnuToolchainBin).ProviderPath
    if (-not (Test-Path -LiteralPath $GnuToolchainBin -PathType Container)) {
        throw '-GnuToolchainBin must name an existing compiler bin directory.'
    }
}

function New-LocalSecret {
    param([int]$ByteCount)
    $bytes = New-Object byte[] $ByteCount
    $random = [Security.Cryptography.RandomNumberGenerator]::Create()
    try { $random.GetBytes($bytes) } finally { $random.Dispose() }
    return [BitConverter]::ToString($bytes).Replace('-', '').ToLowerInvariant()
}

function Invoke-LocalDocker {
    param([string[]]$DockerArguments)
    & docker @DockerArguments
    if ($LASTEXITCODE -ne 0) {
        $script:exitCode = $LASTEXITCODE
        throw "Docker failed with exit code $LASTEXITCODE."
    }
}

function Initialize-Msvc {
    if ($env:VSCMD_ARG_TGT_ARCH -eq 'x64' -and $env:VCToolsInstallDir -and
        (Test-Path -LiteralPath (Join-Path $env:VCToolsInstallDir 'bin\Hostx64\x64\link.exe'))) {
        return
    }
    $programFilesX86 = [Environment]::GetFolderPath('ProgramFilesX86')
    $vswhere = Join-Path $programFilesX86 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) {
        throw 'Install Visual Studio C++ build tools and a Windows SDK, or supply -GnuToolchainBin.'
    }
    $installation = (& $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath | Select-Object -First 1)
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($installation)) {
        throw 'Visual Studio C++ tools were not found. Install them or supply -GnuToolchainBin.'
    }
    $developerCommand = Join-Path $installation 'Common7\Tools\VsDevCmd.bat'
    $commandLine = '"{0}" -no_logo -arch=x64 -host_arch=x64 && set' -f $developerCommand
    $developerEnvironment = & $env:ComSpec /d /s /c $commandLine
    if ($LASTEXITCODE -ne 0) { throw 'Could not initialize the Visual Studio build environment.' }
    foreach ($entry in $developerEnvironment) {
        $separator = $entry.IndexOf('=')
        if ($separator -gt 0) {
            [Environment]::SetEnvironmentVariable($entry.Substring(0, $separator), $entry.Substring($separator + 1), 'Process')
        }
    }
}

$savedEnvironment = [Environment]::GetEnvironmentVariables('Process')
$composeArguments = @('compose', '--project-name', 'bucket-mount-rustfs', '--env-file', $credentialFile, '--file', $composeFile)
$startAttempted = $false
$containerStarted = $false
$ready = $false
$exitCode = 0
Push-Location -LiteralPath $repositoryRoot
try {
    Get-Command docker -ErrorAction Stop | Out-Null
    Invoke-LocalDocker -DockerArguments @('compose', 'version')
    if (-not (Test-Path -LiteralPath $credentialFile -PathType Leaf)) {
        New-Item -ItemType Directory -Path $localDirectory -Force | Out-Null
        $credentials = 'RUSTFS_ACCESS_KEY=' + (New-LocalSecret -ByteCount 10) + "`n" +
            'RUSTFS_SECRET_KEY=' + (New-LocalSecret -ByteCount 32) + "`n"
        # CreateNew preserves existing local credentials if another process raced us.
        $stream = [IO.File]::Open($credentialFile, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try {
            $encoded = [Text.Encoding]::ASCII.GetBytes($credentials)
            $stream.Write($encoded, 0, $encoded.Length)
            $stream.Flush($true)
        } finally { $stream.Dispose() }
        Write-Output 'Created local RustFS test credentials in .local/rustfs.env.'
    }

    $localCredentials = @{}
    foreach ($line in (Get-Content -LiteralPath $credentialFile)) {
        if ($line -match '^(RUSTFS_ACCESS_KEY|RUSTFS_SECRET_KEY)=([A-Za-z0-9]+)$') {
            $localCredentials[$Matches[1]] = $Matches[2]
        } elseif (-not [string]::IsNullOrWhiteSpace($line) -and -not $line.StartsWith('#')) {
            throw 'Unexpected format in .local/rustfs.env; use only the two generated local credential entries.'
        }
    }
    if (-not $localCredentials.ContainsKey('RUSTFS_ACCESS_KEY') -or
        -not $localCredentials.ContainsKey('RUSTFS_SECRET_KEY')) {
        throw '.local/rustfs.env must contain both local RustFS credential entries.'
    }
    # Process variables override Compose --env-file values. Set both explicitly so
    # an unrelated shell environment cannot silently change the test credentials.
    $env:RUSTFS_ACCESS_KEY = $localCredentials['RUSTFS_ACCESS_KEY']
    $env:RUSTFS_SECRET_KEY = $localCredentials['RUSTFS_SECRET_KEY']
    $env:BUCKET_MOUNT_TEST_ENDPOINT = 'http://127.0.0.1:19000'
    $env:BUCKET_MOUNT_TEST_BUCKET = 'bucket-mount-test'
    $env:BUCKET_MOUNT_TEST_ACCESS_KEY = $localCredentials['RUSTFS_ACCESS_KEY']
    $env:BUCKET_MOUNT_TEST_SECRET_KEY = $localCredentials['RUSTFS_SECRET_KEY']

    $startAttempted = $true
    Invoke-LocalDocker -DockerArguments ($composeArguments + @('up', '--detach', 'rustfs'))
    $containerStarted = $true
    $deadline = [DateTime]::UtcNow.AddSeconds(90)
    while ([DateTime]::UtcNow -lt $deadline) {
        try {
            $response = Invoke-WebRequest -Uri 'http://127.0.0.1:19000/health' -UseBasicParsing -TimeoutSec 2
            if ($response.StatusCode -eq 200) { $ready = $true; break }
        } catch {
            # RustFS may still be initializing its persistent volume.
        }
        Start-Sleep -Seconds 2
    }
    if (-not $ready) {
        throw 'RustFS did not become healthy within 90 seconds. Inspect the bucket-mount-rustfs Compose project logs.'
    }
    Write-Output 'RustFS is ready at http://127.0.0.1:19000; running isolated S3 and Windows sync tests.'

    $testArguments = @('--ignored', '--nocapture', '--test-threads=1')
    if ($TestExecutable) {
        & $TestExecutable @testArguments
    } else {
        Get-Command cargo, rustup -ErrorAction Stop | Out-Null
        $toolchainText = Get-Content -LiteralPath (Join-Path $repositoryRoot 'rust-toolchain.toml') -Raw
        $versionMatch = [regex]::Match($toolchainText, '(?m)^channel\s*=\s*"(\d+\.\d+\.\d+)"\s*$')
        if (-not $versionMatch.Success) { throw 'rust-toolchain.toml must pin a complete Rust version.' }
        $targetTriple = if ($GnuToolchainBin) { 'x86_64-pc-windows-gnu' } else { 'x86_64-pc-windows-msvc' }
        $rustToolchain = $versionMatch.Groups[1].Value + '-' + $targetTriple
        if ($GnuToolchainBin) {
            $env:PATH = "$GnuToolchainBin;$env:PATH"
            if ($env:CARGO_ENCODED_RUSTFLAGS) {
                $env:CARGO_ENCODED_RUSTFLAGS += [char]31 + '-C' + [char]31 + 'link-self-contained=yes'
            } else {
                $env:RUSTFLAGS = (($env:RUSTFLAGS + ' -C link-self-contained=yes').Trim())
            }
        } else {
            Initialize-Msvc
        }
        $env:CARGO_NET_OFFLINE = if ($Offline) { 'true' } else { 'false' }
        $cargoArguments = @('test', '--test', 's3_live', '--locked')
        if ($Offline) { $cargoArguments += '--offline' }
        & cargo "+$rustToolchain" @cargoArguments '--' @testArguments
    }
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) { throw "RustFS integration tests failed with exit code $exitCode." }
} catch {
    if ($exitCode -eq 0) { $exitCode = 1 }
    Write-Error -Message $_ -ErrorAction Continue
} finally {
    if ($startAttempted -and -not $KeepRunning) {
        & docker @composeArguments 'stop' '--timeout' '10' 'rustfs'
        if ($LASTEXITCODE -ne 0) {
            Write-Warning 'Could not stop the bucket-mount-rustfs test container.'
            if ($exitCode -eq 0) { $exitCode = $LASTEXITCODE }
        }
    } elseif ($startAttempted -and $KeepRunning) {
        if ($ready) {
            Write-Output 'RustFS remains running; its console is at http://127.0.0.1:19001. Local credentials are in .local/rustfs.env.'
        } elseif ($containerStarted) {
            Write-Output 'RustFS container retained, but readiness was not confirmed. Check the bucket-mount-rustfs Compose project status and logs.'
        } else {
            Write-Output 'RustFS test resources retained after startup failed. Check the bucket-mount-rustfs Compose project status and logs.'
        }
    }
    $currentEnvironment = [Environment]::GetEnvironmentVariables('Process')
    foreach ($name in $currentEnvironment.Keys) {
        if (-not $savedEnvironment.Contains($name)) {
            [Environment]::SetEnvironmentVariable($name, $null, 'Process')
        }
    }
    foreach ($name in $savedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process')
    }
    Pop-Location
}
exit $exitCode
