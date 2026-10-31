#Requires -Version 5.1
<#
Exercise release packaging without compiling Rust or requiring both MSVC toolsets.
Only Cargo, rustup, and Visual Studio discovery are replaced with fixtures. The
real target selection, payload isolation, PE validation, ZIP, and checksum paths run.
#>
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$fixture = Join-Path $repositoryRoot "target\build-script-test-$([guid]::NewGuid())"
$originalEnvironment = [Environment]::GetEnvironmentVariables('Process')
$buildTest = @{ Calls = @(); WrongArchitecture = $false }

function Assert-True($Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function rustup {
    $global:LASTEXITCODE = 0
    if ($args[0] -eq 'target') {
        'x86_64-pc-windows-msvc'
        'aarch64-pc-windows-msvc'
    } else { 'rustc fixture' }
}

function cargo {
    $global:LASTEXITCODE = 0
    $buildTest.Calls += [pscustomobject]@{ Arguments = @($args); Payload = $env:BUCKET_MOUNT_SETUP_PAYLOAD }
    $operation = $args[1]
    if ($operation -eq 'metadata') {
        @{ target_directory = (Join-Path $fixture 'target'); packages = @(@{ name = 'bucket-mount'; version = '1.2.3' }) } | ConvertTo-Json -Depth 3 -Compress
    } elseif ($operation -eq 'build') {
        $target = $args[[Array]::IndexOf($args, '--target') + 1]
        $bin = $args[[Array]::IndexOf($args, '--bin') + 1]
        $machine = if ($target.StartsWith('aarch64') -and -not $buildTest.WrongArchitecture) { 0xaa64 } else { 0x8664 }
        if ($bin -eq 'bucket-mount') {
            Assert-True (-not $env:BUCKET_MOUNT_SETUP_PAYLOAD) 'Application inherited a stale setup payload.'
        } else {
            $expected = Join-Path $fixture "target\$target\release\bucket-mount.exe"
            Assert-True ($env:BUCKET_MOUNT_SETUP_PAYLOAD -eq $expected) 'Setup embedded a different target application.'
        }
        $directory = Join-Path $fixture "target\$target\release"
        New-Item -ItemType Directory -Path $directory -Force | Out-Null
        $bytes = New-Object byte[] 128
        $bytes[0] = 0x4d; $bytes[1] = 0x5a; $bytes[0x3c] = 64
        $bytes[64] = 0x50; $bytes[65] = 0x45
        [BitConverter]::GetBytes([uint16]$machine).CopyTo($bytes, 68)
        [IO.File]::WriteAllBytes((Join-Path $directory "$bin.exe"), $bytes)
    }
}

try {
    New-Item -ItemType Directory -Path (Join-Path $fixture 'scripts') -Force | Out-Null
    foreach ($name in @('rust-toolchain.toml', 'README.md', 'LICENSE')) {
        Copy-Item -LiteralPath (Join-Path $repositoryRoot $name) -Destination $fixture
    }
    $scriptPath = Join-Path $PSScriptRoot 'build.ps1'
    $tokens = $null; $errors = $null
    $ast = [Management.Automation.Language.Parser]::ParseFile($scriptPath, [ref]$tokens, [ref]$errors)
    Assert-True ($errors.Count -eq 0) 'Build script contains PowerShell syntax errors.'
    $initializer = $ast.Find({ param($node)
        $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Initialize-Msvc'
    }, $true)
    $source = Get-Content -LiteralPath $scriptPath -Raw
    $stub = 'function Initialize-Msvc { param([string]$TargetArchitecture)  }'
    $source = $source.Remove($initializer.Extent.StartOffset, $initializer.Extent.EndOffset - $initializer.Extent.StartOffset).Insert($initializer.Extent.StartOffset, $stub)
    $build = Join-Path $fixture 'scripts\build.ps1'
    [IO.File]::WriteAllText($build, $source)
    $env:PROCESSOR_ARCHITECTURE = 'AMD64'
    Remove-Item Env:PROCESSOR_ARCHITEW6432 -ErrorAction SilentlyContinue
    $env:BUCKET_MOUNT_SETUP_PAYLOAD = 'caller-payload'
    $output = Join-Path $fixture 'packages with spaces'
    & $build -Offline -OutputDirectory $output
    Assert-True ($env:BUCKET_MOUNT_SETUP_PAYLOAD -eq 'caller-payload') 'Caller environment was not restored.'
    Assert-True (@(Get-ChildItem -LiteralPath $output -File).Count -eq 8) 'Expected two EXEs, two ZIPs, and four checksums.'
    $tests = @($buildTest.Calls | Where-Object { $_.Arguments[1] -eq 'test' })
    Assert-True ($tests.Count -eq 2) 'Both targets must compile tests.'
    Assert-True ($tests[0].Arguments -notcontains '--no-run') 'Native x64 tests must execute.'
    Assert-True ($tests[1].Arguments -contains '--no-run') 'Cross-compiled ARM64 tests must not execute on x64.'
    foreach ($checksum in Get-ChildItem -LiteralPath $output -Filter '*.sha256') {
        $text = [IO.File]::ReadAllText($checksum.FullName)
        Assert-True (-not $text.Contains("`r")) 'Checksums must use LF for Linux release verification.'
        $digest, $filename = $text.TrimEnd() -split '  ', 2
        Assert-True ((Get-FileHash -LiteralPath (Join-Path $output $filename)).Hash.ToLowerInvariant() -eq $digest) 'Package checksum mismatch.'
    }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    foreach ($arch in @('x64', 'arm64')) {
        $zip = [IO.Compression.ZipFile]::OpenRead((Join-Path $output "bucket-mount-1.2.3-windows-$arch-portable.zip"))
        try {
            $entry = $zip.GetEntry('bucket-mount.exe')
            Assert-True ($null -ne $entry) 'Portable archive is missing the executable.'
            $reader = New-Object IO.BinaryReader($entry.Open())
            try {
                $bytes = $reader.ReadBytes(128)
                $expected = if ($arch -eq 'arm64') { 0xaa64 } else { 0x8664 }
                Assert-True ([BitConverter]::ToUInt16($bytes, 68) -eq $expected) 'Portable ZIP has the wrong architecture.'
            } finally { $reader.Dispose() }
        } finally { $zip.Dispose() }
    }
    $buildTest.Calls = @()
    $env:PROCESSOR_ARCHITECTURE = 'ARM64'
    & $build -Architecture arm64 -Offline -OutputDirectory $output
    $nativeTest = @($buildTest.Calls | Where-Object { $_.Arguments[1] -eq 'test' })
    Assert-True ($nativeTest[0].Arguments -notcontains '--no-run') 'Native ARM64 tests must execute.'
    Assert-True ($nativeTest[0].Arguments[0] -like '*-aarch64-pc-windows-msvc') 'ARM64 host toolchain was not selected.'
    $buildTest.WrongArchitecture = $true
    $rejected = $false
    try { & $build -Architecture arm64 -SkipChecks -Offline -OutputDirectory $output }
    catch { $rejected = $_.Exception.Message -like '*Executable is not arm64*' }
    Assert-True $rejected 'Packaging accepted an x64 application for an ARM64 release.'
    Assert-True ($env:BUCKET_MOUNT_SETUP_PAYLOAD -eq 'caller-payload') 'Failed build did not restore caller environment.'
    Write-Output 'Release packaging tests passed.'
} finally {
    foreach ($name in [Environment]::GetEnvironmentVariables('Process').Keys) {
        if (-not $originalEnvironment.Contains($name)) { [Environment]::SetEnvironmentVariable($name, $null, 'Process') }
    }
    foreach ($name in $originalEnvironment.Keys) { [Environment]::SetEnvironmentVariable($name, $originalEnvironment[$name], 'Process') }
    $targetRoot = [IO.Path]::GetFullPath((Join-Path $repositoryRoot 'target')) + [IO.Path]::DirectorySeparatorChar
    $resolvedFixture = [IO.Path]::GetFullPath($fixture)
    if (-not $resolvedFixture.StartsWith($targetRoot, [StringComparison]::OrdinalIgnoreCase)) { throw 'Test cleanup path escaped target directory.' }
    if (Test-Path -LiteralPath $resolvedFixture) { Remove-Item -LiteralPath $resolvedFixture -Recurse -Force }
}
