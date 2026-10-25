#Requires -Version 5.1

<#
.SYNOPSIS
Checks, builds, and packages the pinned Windows x64 release.
.DESCRIPTION
Builds the app first, then embeds it in the native Rust setup executable. Produces
a standalone setup EXE and a portable ZIP, each with a SHA256 checksum.
Requires rustup and the pinned Rust toolchain, plus Visual Studio C++ build tools
and a Windows SDK. Pass -GnuToolchainBin to use an installed LLVM MinGW toolchain
instead. Dependencies may be downloaded unless -Offline is supplied.
.PARAMETER SkipChecks
Skip formatting, type checking, Clippy, and tests when they already passed.
.EXAMPLE
.\scripts\build.ps1
.EXAMPLE
.\scripts\build.ps1 -GnuToolchainBin 'C:\tools\llvm-mingw\bin' -Offline
#>
[CmdletBinding()]
param(
    [string]$GnuToolchainBin,
    [switch]$Offline,
    [switch]$SkipChecks,
    [string]$OutputDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'This build script requires Windows.'
}

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if (-not $OutputDirectory) {
    $OutputDirectory = Join-Path $repositoryRoot 'dist'
} else {
    # Resolve relative paths against the caller's PowerShell location before cd.
    $OutputDirectory = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDirectory)
    if (-not [IO.Path]::IsPathRooted($OutputDirectory)) {
        throw '-OutputDirectory must be a filesystem directory.'
    }
}
$toolchainText = Get-Content -LiteralPath (Join-Path $repositoryRoot 'rust-toolchain.toml') -Raw
$versionMatch = [regex]::Match($toolchainText, '(?m)^channel\s*=\s*"(\d+\.\d+\.\d+)"\s*$')
if (-not $versionMatch.Success) {
    throw 'rust-toolchain.toml must pin a complete Rust version.'
}
$rustVersion = $versionMatch.Groups[1].Value
$targetTriple = if ($GnuToolchainBin) { 'x86_64-pc-windows-gnu' } else { 'x86_64-pc-windows-msvc' }
$rustToolchain = "$rustVersion-$targetTriple"

function Invoke-Cargo {
    param([string[]]$CargoArguments)
    & cargo "+$rustToolchain" @CargoArguments
    if ($LASTEXITCODE -ne 0) {
        throw "Cargo failed ($LASTEXITCODE): $($CargoArguments -join ' ')"
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
        throw 'Install Visual Studio Build Tools with Desktop development with C++ and a Windows SDK, then rerun this script.'
    }
    $installation = (& $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath | Select-Object -First 1)
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($installation)) {
        throw 'Visual Studio C++ tools were not found. Install Desktop development with C++ and a Windows SDK.'
    }
    $developerCommand = Join-Path $installation 'Common7\Tools\VsDevCmd.bat'
    # Capture the developer environment without logging environment variable values.
    $commandLine = '"{0}" -no_logo -arch=x64 -host_arch=x64 && set' -f $developerCommand
    $developerEnvironment = & $env:ComSpec /d /s /c $commandLine
    if ($LASTEXITCODE -ne 0) {
        throw 'Visual Studio could not initialize its x64 build environment.'
    }
    foreach ($entry in $developerEnvironment) {
        $separator = $entry.IndexOf('=')
        if ($separator -gt 0) {
            [Environment]::SetEnvironmentVariable($entry.Substring(0, $separator), $entry.Substring($separator + 1), 'Process')
        }
    }
}

$savedEnvironment = [Environment]::GetEnvironmentVariables('Process')
Push-Location -LiteralPath $repositoryRoot
try {
    Get-Command rustup, cargo -ErrorAction Stop | Out-Null
    & rustup run $rustToolchain rustc --version
    if ($LASTEXITCODE -ne 0) {
        throw "Install the toolchain first: rustup toolchain install $rustToolchain --profile minimal --component rustfmt --component clippy"
    }

    if ($GnuToolchainBin) {
        $compilerDirectory = (Resolve-Path -LiteralPath $GnuToolchainBin).ProviderPath
        if (-not (Test-Path -LiteralPath $compilerDirectory -PathType Container)) {
            throw '-GnuToolchainBin must name the compiler bin directory.'
        }
        $compiler = @('x86_64-w64-mingw32-gcc.exe', 'x86_64-w64-mingw32-clang.exe', 'gcc.exe') |
            ForEach-Object { Join-Path $compilerDirectory $_ } |
            Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
            Select-Object -First 1
        $archiver = @('x86_64-w64-mingw32-ar.exe', 'llvm-ar.exe', 'ar.exe') |
            ForEach-Object { Join-Path $compilerDirectory $_ } |
            Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
            Select-Object -First 1
        if (-not $compiler -or -not $archiver) {
            throw 'The selected GNU toolchain needs an x64 MinGW compiler and ar/llvm-ar.'
        }
        $env:PATH = "$compilerDirectory;$env:PATH"
        $env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = $compiler
        $env:CC_x86_64_pc_windows_gnu = $compiler
        $env:AR_x86_64_pc_windows_gnu = $archiver
        # Rust's GNU runtime supplies compatible CRT/GCC libraries for LLVM MinGW.
        if ($env:CARGO_ENCODED_RUSTFLAGS) {
            $env:CARGO_ENCODED_RUSTFLAGS += [char]31 + '-C' + [char]31 + 'link-self-contained=yes'
        } else {
            $env:RUSTFLAGS = (($env:RUSTFLAGS + ' -C link-self-contained=yes').Trim())
        }
    } else {
        Initialize-Msvc
    }

    $env:CARGO_NET_OFFLINE = if ($Offline) { 'true' } else { 'false' }
    # Checks and the app build must not embed an inherited or stale setup payload.
    # The caller's original value is restored with the rest of the environment.
    # Remove-Item deletes the variable in every PowerShell version; the .NET setter
    # can leave it defined but empty, which the build script would read as a path.
    Remove-Item -LiteralPath Env:BUCKET_MOUNT_SETUP_PAYLOAD -ErrorAction SilentlyContinue
    if (Test-Path Env:BUCKET_MOUNT_SETUP_PAYLOAD) {
        throw 'BUCKET_MOUNT_SETUP_PAYLOAD must be unset while checking and building the application.'
    }
    # Build for this explicitly selected native toolchain. In GNU mode omitting
    # --target also applies RUSTFLAGS to host build scripts and proc macros.
    $lockedArguments = @('--locked')
    if ($Offline) { $lockedArguments += '--offline' }
    Invoke-Cargo -CargoArguments (@('fetch', '--target', $targetTriple) + $lockedArguments)
    if (-not $SkipChecks) {
        Invoke-Cargo -CargoArguments @('fmt', '--all', '--', '--check')
        Invoke-Cargo -CargoArguments (@('check', '--all-targets') + $lockedArguments)
        Invoke-Cargo -CargoArguments (@('clippy', '--all-targets') + $lockedArguments + @('--', '-D', 'warnings'))
        # Ignored CFAPI tests require a suitable interactive Windows host and are opt-in.
        Invoke-Cargo -CargoArguments (@('test', '--all-targets') + $lockedArguments)
    }
    Invoke-Cargo -CargoArguments (@('build', '--release', '--bin', 'bucket-mount') + $lockedArguments)

    $metadataArguments = @('metadata', '--no-deps', '--format-version', '1', '--locked')
    if ($Offline) { $metadataArguments += '--offline' }
    $metadataText = & cargo "+$rustToolchain" @metadataArguments | Out-String
    if ($LASTEXITCODE -ne 0) { throw 'Could not read Cargo package metadata.' }
    $metadata = $metadataText | ConvertFrom-Json
    $package = $metadata.packages | Where-Object { $_.name -eq 'bucket-mount' } | Select-Object -First 1
    if (-not $package) { throw 'Cargo metadata did not contain bucket-mount.' }
    $executable = Join-Path $metadata.target_directory 'release\bucket-mount.exe'
    if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
        throw "Cannot build setup; application executable is missing: $executable"
    }
    $env:BUCKET_MOUNT_SETUP_PAYLOAD = [IO.Path]::GetFullPath($executable)
    Invoke-Cargo -CargoArguments (@('build', '--release', '--bin', 'bucket-mount-setup') + $lockedArguments)
    $setupExecutable = Join-Path $metadata.target_directory 'release\bucket-mount-setup.exe'
    $readme = Join-Path $repositoryRoot 'README.md'
    foreach ($requiredFile in @($executable, $setupExecutable, $readme)) {
        if (-not (Test-Path -LiteralPath $requiredFile -PathType Leaf)) {
            throw "Cannot package the release; file is missing: $requiredFile"
        }
    }
    $OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
    New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
    $variant = if ($GnuToolchainBin) { 'windows-x64-gnu' } else { 'windows-x64' }
    $archive = Join-Path $OutputDirectory "bucket-mount-$($package.version)-$variant-portable.zip"
    $standaloneSetup = Join-Path $OutputDirectory "bucket-mount-$($package.version)-$variant-setup.exe"
    Copy-Item -LiteralPath $setupExecutable -Destination $standaloneSetup -Force
    $setupDigest = (Get-FileHash -LiteralPath $standaloneSetup -Algorithm SHA256).Hash.ToLowerInvariant()
    # Use LF so sha256sum can verify these files on Linux release runners too.
    [IO.File]::WriteAllText("$standaloneSetup.sha256", "$setupDigest  $([IO.Path]::GetFileName($standaloneSetup))`n", [Text.Encoding]::ASCII)
    $packageFiles = @($executable, $readme)
    foreach ($optionalName in @('LICENSE')) {
        $optionalFile = Join-Path $repositoryRoot $optionalName
        if (Test-Path -LiteralPath $optionalFile -PathType Leaf) { $packageFiles += $optionalFile }
    }
    Compress-Archive -LiteralPath $packageFiles -DestinationPath $archive -CompressionLevel Optimal -Force
    $digest = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText("$archive.sha256", "$digest  $([IO.Path]::GetFileName($archive))`n", [Text.Encoding]::ASCII)
    Write-Output "Release: $archive"
    Write-Output "SHA256:  $digest"
    Write-Output "Setup:   $standaloneSetup"
    Write-Output "SHA256:  $setupDigest"
} finally {
    # Restore the caller's process environment; never write user/system settings here.
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
