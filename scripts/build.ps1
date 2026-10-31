#Requires -Version 5.1

<#
.SYNOPSIS
Checks, builds, and packages Windows x64 and ARM64 releases.
.DESCRIPTION
Builds the app first, then embeds it in the native Rust setup executable. Produces
a standalone setup EXE and a portable ZIP, each with a SHA256 checksum.
Requires rustup and the pinned Rust toolchain, plus Visual Studio C++ build tools
and a Windows SDK. Pass -GnuToolchainBin to use an installed LLVM MinGW toolchain
instead. Dependencies may be downloaded unless -Offline is supplied.
ARM64 also needs the Visual Studio C++ Clang compiler or LLVM's clang-cl on PATH.
.PARAMETER Architecture
Targets to package. Defaults to x64 and arm64 for MSVC, or x64 for GNU.
Cross builds compile tests; native builds also run them. CI tests both natively.
.PARAMETER SkipChecks
Skip formatting, type checking, Clippy, and tests when they already passed.
.EXAMPLE
.\scripts\build.ps1
.EXAMPLE
.\scripts\build.ps1 -GnuToolchainBin 'C:\tools\llvm-mingw\bin' -Offline
#>
[CmdletBinding()]
param(
    [ValidateSet('x64', 'arm64')]
    [string[]]$Architecture,
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
if (-not $Architecture) {
    $Architecture = if ($GnuToolchainBin) { @('x64') } else { @('x64', 'arm64') }
}
$Architecture = @($Architecture | ForEach-Object { $_.ToLowerInvariant() } | Select-Object -Unique)
if ($GnuToolchainBin -and $Architecture -contains 'arm64') {
    throw 'Windows ARM64 requires MSVC. Omit -GnuToolchainBin to build ARM64.'
}
$nativeArchitecture = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
$hostArchitecture = switch ($nativeArchitecture) {
    'AMD64' { 'x64' }
    'ARM64' { 'arm64' }
    default { throw "Unsupported Windows build host: $nativeArchitecture" }
}
$hostTriple = if ($GnuToolchainBin) { 'x86_64-pc-windows-gnu' } elseif ($hostArchitecture -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
$rustToolchain = "$rustVersion-$hostTriple"

function Invoke-Cargo {
    param([string[]]$CargoArguments)
    & cargo "+$rustToolchain" @CargoArguments
    if ($LASTEXITCODE -ne 0) {
        throw "Cargo failed ($LASTEXITCODE): $($CargoArguments -join ' ')"
    }
}

function Initialize-Msvc {
    param([string]$TargetArchitecture)
    $linkerRelative = "bin\Host$hostArchitecture\$TargetArchitecture\link.exe"
    if ($env:VSCMD_ARG_TGT_ARCH -eq $TargetArchitecture -and $env:VCToolsInstallDir -and
        (Test-Path -LiteralPath (Join-Path $env:VCToolsInstallDir $linkerRelative))) {
        if ($TargetArchitecture -eq 'arm64') { Initialize-Arm64Compiler }
        return
    }
    $programFilesX86 = [Environment]::GetFolderPath('ProgramFilesX86')
    $vswhere = Join-Path $programFilesX86 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) {
        throw 'Install Visual Studio Build Tools with Desktop development with C++ and a Windows SDK, then rerun this script.'
    }
    $component = if ($TargetArchitecture -eq 'arm64') { 'Microsoft.VisualStudio.Component.VC.Tools.ARM64' } else { 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64' }
    $installation = (& $vswhere -latest -products '*' -requires $component -property installationPath | Select-Object -First 1)
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($installation)) {
        throw "Visual Studio C++ tools for $TargetArchitecture were not found. Install $component and a Windows SDK."
    }
    $developerCommand = Join-Path $installation 'Common7\Tools\VsDevCmd.bat'
    # Capture the developer environment without logging environment variable values.
    $commandLine = '"{0}" -no_logo -arch={1} -host_arch={2} && set' -f $developerCommand, $TargetArchitecture, $hostArchitecture
    $developerEnvironment = & $env:ComSpec /d /s /c $commandLine
    if ($LASTEXITCODE -ne 0) {
        throw "Visual Studio could not initialize its $TargetArchitecture build environment."
    }
    foreach ($entry in $developerEnvironment) {
        $separator = $entry.IndexOf('=')
        if ($separator -gt 0) {
            [Environment]::SetEnvironmentVariable($entry.Substring(0, $separator), $entry.Substring($separator + 1), 'Process')
        }
    }
    if (-not $env:VCToolsInstallDir -or -not (Test-Path -LiteralPath (Join-Path $env:VCToolsInstallDir $linkerRelative))) {
        throw "Visual Studio is missing its $hostArchitecture to $TargetArchitecture compiler tools."
    }
    if ($TargetArchitecture -eq 'arm64') { Initialize-Arm64Compiler }
}

function Initialize-Arm64Compiler {
    # aws-lc-sys requires clang-cl for Windows ARM64. Use target-specific variables
    # so x64 host build scripts and subsequent x64 builds retain their compiler.
    if ($env:CC_aarch64_pc_windows_msvc) { return }
    $command = Get-Command clang-cl.exe -ErrorAction SilentlyContinue
    $candidates = @()
    if ($command) { $candidates += $command.Source }
    if ($env:VCINSTALLDIR) {
        foreach ($relative in @("Tools\Llvm\$hostArchitecture\bin\clang-cl.exe", 'Tools\Llvm\bin\clang-cl.exe')) {
            $candidates += Join-Path $env:VCINSTALLDIR $relative
        }
    }
    $candidates += Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'LLVM\bin\clang-cl.exe'
    $compiler = $candidates | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
    if (-not $compiler) {
        throw 'Windows ARM64 crypto dependencies require clang-cl. Install the Visual Studio C++ Clang Compiler for Windows component or add LLVM to PATH.'
    }
    $env:CC_aarch64_pc_windows_msvc = $compiler
}

function Restore-Environment {
    $currentEnvironment = [Environment]::GetEnvironmentVariables('Process')
    foreach ($name in $currentEnvironment.Keys) {
        if (-not $savedEnvironment.Contains($name)) {
            [Environment]::SetEnvironmentVariable($name, $null, 'Process')
        }
    }
    foreach ($name in $savedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process')
    }
}

function Assert-PeArchitecture {
    param([string]$Path, [string]$TargetArchitecture)
    $stream = [IO.File]::OpenRead($Path)
    $reader = New-Object IO.BinaryReader($stream)
    try {
        if ($stream.Length -lt 64 -or $reader.ReadUInt16() -ne 0x5a4d) { throw "Invalid Windows executable: $Path" }
        $stream.Position = 0x3c
        $peOffset = $reader.ReadUInt32()
        if ($peOffset -lt 64 -or $peOffset + 6 -gt $stream.Length) { throw "Invalid PE header: $Path" }
        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x4550) { throw "Invalid PE signature: $Path" }
        $expected = if ($TargetArchitecture -eq 'arm64') { 0xaa64 } else { 0x8664 }
        if ($reader.ReadUInt16() -ne $expected) { throw "Executable is not $TargetArchitecture`: $Path" }
    } finally {
        $reader.Dispose()
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
    $installedTargets = @(& rustup target list --installed --toolchain $rustToolchain)
    if ($LASTEXITCODE -ne 0) { throw 'Could not read installed Rust targets.' }
    if (-not $SkipChecks) { Invoke-Cargo -CargoArguments @('fmt', '--all', '--', '--check') }

    foreach ($arch in $Architecture) {
        # Isolate SDK paths, compiler flags, and setup payloads between targets.
        Restore-Environment
        $targetTriple = if ($GnuToolchainBin) { 'x86_64-pc-windows-gnu' } elseif ($arch -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
        if ($installedTargets -notcontains $targetTriple) {
            throw "Install the target first: rustup target add $targetTriple --toolchain $rustToolchain"
        }
        $targetArguments = @('--target', $targetTriple)
        if ($GnuToolchainBin) {
            # GNU also needs link-self-contained on host build scripts/proc macros.
            $targetArguments = @()
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
            Initialize-Msvc -TargetArchitecture $arch
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
        Write-Output "Building Windows $arch ($targetTriple)"
        $lockedArguments = @('--locked')
        if ($Offline) { $lockedArguments += '--offline' }
        Invoke-Cargo -CargoArguments (@('fetch', '--target', $targetTriple) + $lockedArguments)
        if (-not $SkipChecks) {
            Invoke-Cargo -CargoArguments (@('check', '--all-targets') + $targetArguments + $lockedArguments)
            Invoke-Cargo -CargoArguments (@('clippy', '--all-targets') + $targetArguments + $lockedArguments + @('--', '-D', 'warnings'))
            # Ignored CFAPI tests require a suitable interactive Windows host and are opt-in.
            $testArguments = @('test', '--all-targets') + $targetArguments + $lockedArguments
            if ($arch -ne $hostArchitecture) {
                $testArguments += '--no-run'
                Write-Output "Cross build: compiling $arch tests; CI runs them on a native runner."
            }
            Invoke-Cargo -CargoArguments $testArguments
        }
        Invoke-Cargo -CargoArguments (@('build', '--release', '--bin', 'bucket-mount') + $targetArguments + $lockedArguments)

        $metadataArguments = @('metadata', '--no-deps', '--format-version', '1', '--locked')
        if ($Offline) { $metadataArguments += '--offline' }
        $metadataText = & cargo "+$rustToolchain" @metadataArguments | Out-String
        if ($LASTEXITCODE -ne 0) { throw 'Could not read Cargo package metadata.' }
        $metadata = $metadataText | ConvertFrom-Json
        $package = $metadata.packages | Where-Object { $_.name -eq 'bucket-mount' } | Select-Object -First 1
        if (-not $package) { throw 'Cargo metadata did not contain bucket-mount.' }
        $releaseDirectory = if ($GnuToolchainBin) { Join-Path $metadata.target_directory 'release' } else { Join-Path $metadata.target_directory "$targetTriple\release" }
        $executable = Join-Path $releaseDirectory 'bucket-mount.exe'
        if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
            throw "Cannot build setup; application executable is missing: $executable"
        }
        Assert-PeArchitecture -Path $executable -TargetArchitecture $arch
        $env:BUCKET_MOUNT_SETUP_PAYLOAD = [IO.Path]::GetFullPath($executable)
        Invoke-Cargo -CargoArguments (@('build', '--release', '--bin', 'bucket-mount-setup') + $targetArguments + $lockedArguments)
        $setupExecutable = Join-Path $releaseDirectory 'bucket-mount-setup.exe'
        Assert-PeArchitecture -Path $setupExecutable -TargetArchitecture $arch
        $readme = Join-Path $repositoryRoot 'README.md'
        foreach ($requiredFile in @($executable, $setupExecutable, $readme)) {
            if (-not (Test-Path -LiteralPath $requiredFile -PathType Leaf)) {
                throw "Cannot package the release; file is missing: $requiredFile"
            }
        }
        $OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
        New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
        $variant = if ($GnuToolchainBin) { 'windows-x64-gnu' } else { "windows-$arch" }
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
    }
} finally {
    # Restore the caller's process environment; never write user/system settings here.
    Restore-Environment
    Pop-Location
}
