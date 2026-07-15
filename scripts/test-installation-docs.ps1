$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $PSScriptRoot
$DocsPath = Join-Path $Root "docs/installation.md"
$CargoPath = Join-Path $Root "Cargo.toml"
$Temp = Join-Path ([System.IO.Path]::GetTempPath()) "bitbygit-install-$([guid]::NewGuid())"

function Fail([string] $Message) {
    throw "installation docs validation failed: $Message"
}

function Get-Block([string] $Heading, [string] $Language) {
    $Pattern = '(?ms)^## {0}\r?\n.*?^```{1}\r?\n(.*?)^```' -f [regex]::Escape($Heading), [regex]::Escape($Language)
    $Match = [regex]::Match($script:DocsText, $Pattern)
    if (-not $Match.Success) { Fail "missing $Heading $Language block" }
    return $Match.Groups[1].Value
}

function ConvertTo-IsolatedPathBlock([string] $Block) {
    $GetUserPath = 'GetEnvironmentVariable("Path", "User")'
    $SetUserPath = '[Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")'
    if (-not $Block.Contains($GetUserPath) -or -not $Block.Contains($SetUserPath)) {
        Fail "Windows block does not contain the expected user PATH operations"
    }

    $Isolated = $Block.Replace($GetUserPath, 'GetEnvironmentVariable("BITBYGIT_TEST_USER_PATH", "Process")')
    $Isolated = $Isolated.Replace($SetUserPath, '$script:CapturedUserPath = $NewUserPath')
    if ($Isolated.Contains('"User"')) {
        Fail "Windows block still accesses persistent user PATH"
    }
    return $Isolated
}

$DocsText = Get-Content -Raw $DocsPath
$CargoText = Get-Content -Raw $CargoPath
$VersionMatch = [regex]::Match($CargoText, '(?ms)^\[workspace\.package\]\r?\n.*?^version = "([^"]+)"')
if (-not $VersionMatch.Success) { Fail "workspace version was not found" }
$Version = $VersionMatch.Groups[1].Value
if (-not $DocsText.Contains("`$Version = `"$Version`"")) {
    Fail "PowerShell examples do not use workspace version $Version"
}
$EmptyPathGuard = '$NewUserPath = if ([string]::IsNullOrEmpty($UserPath)) { $InstallDir } else { "$UserPath;$InstallDir" }'
if ([regex]::Matches($DocsText, [regex]::Escape($EmptyPathGuard)).Count -ne 2) {
    Fail "Windows examples do not handle empty user PATH values consistently"
}
$ProcessPathGuard = '$env:Path = if ([string]::IsNullOrEmpty($env:Path)) { $InstallDir } else { "$InstallDir;$env:Path" }'
if ([regex]::Matches($DocsText, [regex]::Escape($ProcessPathGuard)).Count -ne 2) {
    Fail "Windows examples do not handle process PATH values consistently"
}
if ([regex]::Matches($DocsText, '(?m)^bitbygit --version\r?$').Count -ne 2) {
    Fail "Windows examples do not finish with PATH-resolved version output"
}

foreach ($Match in [regex]::Matches($DocsText, '(?ms)^```powershell\r?\n(.*?)^```')) {
    $Tokens = $null
    $Errors = $null
    [System.Management.Automation.Language.Parser]::ParseInput(
        $Match.Groups[1].Value,
        [ref] $Tokens,
        [ref] $Errors
    ) | Out-Null
    if ($Errors.Count -ne 0) { Fail "PowerShell snippet has syntax errors: $($Errors -join '; ')" }
}

$InstallBlock = ConvertTo-IsolatedPathBlock (Get-Block "Windows x86-64 archive" "powershell")
if (-not $InstallBlock.Contains('$Package = "bitbygit-$Version-$Target"')) {
    Fail "Windows package directory does not match the release layout"
}
if (-not $InstallBlock.Contains('& $InstalledBinary --version')) {
    Fail "Windows installation does not validate the installed path"
}
$SourceBlock = ConvertTo-IsolatedPathBlock (Get-Block "Build from source" "powershell")
if (-not $SourceBlock.Contains('git -C $SourceDir fetch --depth 1 https://github.com/cosentinode/bitbygit.git "${Tag}:${Tag}"') -or
    -not $SourceBlock.Contains('git -C $SourceDir checkout --detach $TagCommit') -or
    -not $SourceBlock.Contains('$HeadCommit -ne $TagCommit')) {
    Fail "PowerShell source build is not pinned to the selected tag"
}

New-Item -ItemType Directory -Path $Temp | Out-Null
$OriginalLocalAppData = $env:LOCALAPPDATA
$OriginalProcessPath = $env:Path

try {
    cargo build --locked --release -p bitbygit
    if ($LASTEXITCODE -ne 0) { Fail "cargo build failed" }

    $Target = "x86_64-pc-windows-msvc"
    $Archive = "bitbygit-$Version-$Target.zip"
    $Package = "bitbygit-$Version-$Target"
    $Assets = Join-Path $Temp "assets"
    $PackageDir = Join-Path $Assets $Package
    New-Item -ItemType Directory -Path $PackageDir | Out-Null
    Copy-Item (Join-Path $Root "target/release/bitbygit.exe") $PackageDir
    Compress-Archive -Path $PackageDir -DestinationPath (Join-Path $Assets $Archive)
    $Digest = (Get-FileHash -Algorithm SHA256 (Join-Path $Assets $Archive)).Hash
    Set-Content -Path (Join-Path $Assets "SHA256SUMS") -Value "$Digest  $Archive"

    $SuccessDir = Join-Path $Temp "success"
    New-Item -ItemType Directory -Path $SuccessDir | Out-Null
    $SuccessScript = Join-Path $SuccessDir "install.ps1"
    Set-Content -Path $SuccessScript -Value $InstallBlock
    $env:LOCALAPPDATA = Join-Path $SuccessDir "local-app-data"
    $env:Path = $null
    Remove-Item Env:BITBYGIT_TEST_USER_PATH -ErrorAction SilentlyContinue
    $CapturedUserPath = $null
    $env:MOCK_DOWNLOAD_DIR = $Assets
    $ErrorActionPreference = "Continue"

    function Invoke-WebRequest {
        param($Uri, $OutFile)
        Copy-Item (Join-Path $env:MOCK_DOWNLOAD_DIR ([System.IO.Path]::GetFileName([uri] $Uri))) $OutFile
    }

    Push-Location $SuccessDir
    try {
        . $SuccessScript
    } finally {
        Pop-Location
    }

    $InstallDir = Join-Path $env:LOCALAPPDATA "Programs\bitbygit"
    $InstalledBinary = Join-Path $InstallDir "bitbygit.exe"
    if ($ErrorActionPreference -ne "Continue") { Fail "archive block changed the caller error preference" }
    if ($env:Path -ne $InstallDir) { Fail "archive block malformed an empty process PATH" }
    if ($CapturedUserPath -ne $InstallDir) { Fail "archive block malformed an empty user PATH" }
    if (-not (Test-Path $InstalledBinary)) { Fail "archive block did not install bitbygit.exe" }
    $ResolvedBinary = (Get-Command bitbygit -CommandType Application).Source
    if ($ResolvedBinary -ne $InstalledBinary) { Fail "archive block did not resolve the installed command through PATH" }
    $Output = bitbygit --version
    if ($LASTEXITCODE -ne 0 -or $Output -ne "bitbygit $Version") {
        Fail "archive block installed the wrong version"
    }

    $FailureAssets = Join-Path $Temp "failure-assets"
    $FailureDir = Join-Path $Temp "failure"
    New-Item -ItemType Directory -Path $FailureAssets, $FailureDir | Out-Null
    Copy-Item (Join-Path $Assets $Archive) $FailureAssets
    Set-Content -Path (Join-Path $FailureAssets "SHA256SUMS") -Value "$('0' * 64)  $Archive"
    $FailureScript = Join-Path $FailureDir "install.ps1"
    Set-Content -Path $FailureScript -Value $InstallBlock
    $env:LOCALAPPDATA = Join-Path $FailureDir "local-app-data"
    $env:Path = $OriginalProcessPath
    $env:MOCK_DOWNLOAD_DIR = $FailureAssets
    $env:MOCK_SIDE_EFFECT = Join-Path $FailureDir "expanded"
    $ErrorActionPreference = "Continue"

    function Expand-Archive {
        Set-Content -Path $env:MOCK_SIDE_EFFECT -Value "expanded"
        throw "archive extraction unexpectedly reached"
    }

    $ChecksumFailed = $false
    Push-Location $FailureDir
    try {
        try {
            . $FailureScript
        } catch {
            $ChecksumFailed = $true
        }
    } finally {
        Pop-Location
    }
    if (-not $ChecksumFailed) { Fail "archive block accepted an invalid checksum" }
    if (Test-Path $env:MOCK_SIDE_EFFECT) { Fail "archive block extracted after checksum failure" }
    if ($ErrorActionPreference -ne "Continue") { Fail "failed archive block changed the caller error preference" }
    if ($env:Path -ne $OriginalProcessPath) { Fail "failed archive block changed the process PATH" }

    function git {
        param([Parameter(ValueFromRemainingArguments = $true)] [string[]] $Arguments)

        $ExpectedTag = "refs/tags/v$Version"
        if ($Arguments.Count -eq 3 -and $Arguments[0] -eq "-C" -and
            $Arguments[2] -eq "init") {
            $BuildDir = Join-Path $Arguments[1] "target/release"
            New-Item -ItemType Directory -Path $BuildDir | Out-Null
            Copy-Item (Join-Path $Root "target/release/bitbygit.exe") $BuildDir
        } elseif ($Arguments.Count -eq 7 -and $Arguments[0] -eq "-C" -and
            $Arguments[2] -eq "fetch" -and $Arguments[3] -eq "--depth" -and
            $Arguments[4] -eq "1" -and
            $Arguments[5] -eq "https://github.com/cosentinode/bitbygit.git" -and
            $Arguments[6] -eq "${ExpectedTag}:${ExpectedTag}") {
            $script:FetchedExactTag = $true
        } elseif ($Arguments.Count -eq 5 -and $Arguments[0] -eq "-C" -and
            $Arguments[2] -eq "rev-parse" -and $Arguments[3] -eq "--verify" -and
            $Arguments[4] -eq "${ExpectedTag}^{commit}" -and $script:FetchedExactTag) {
            return "tag-commit"
        } elseif ($Arguments.Count -eq 5 -and $Arguments[0] -eq "-C" -and
            $Arguments[2] -eq "checkout" -and $Arguments[3] -eq "--detach" -and
            $Arguments[4] -eq "tag-commit" -and $script:FetchedExactTag) {
            $script:CheckedOutExactTag = $true
        } elseif ($Arguments.Count -eq 5 -and $Arguments[0] -eq "-C" -and
            $Arguments[2] -eq "rev-parse" -and $Arguments[3] -eq "--verify" -and
            $Arguments[4] -eq "HEAD" -and $script:CheckedOutExactTag) {
            return "tag-commit"
        } else {
            throw "unexpected git arguments: $Arguments"
        }
        $global:LASTEXITCODE = 0
    }

    function cargo {
        if ($env:MOCK_CARGO_FAILURE -eq "1") {
            $global:LASTEXITCODE = 1
            return
        }
        $global:LASTEXITCODE = 0
    }

    $SourceSuccessDir = Join-Path $Temp "source-success"
    New-Item -ItemType Directory -Path $SourceSuccessDir | Out-Null
    $SourceScript = Join-Path $SourceSuccessDir "install-from-source.ps1"
    Set-Content -Path $SourceScript -Value $SourceBlock
    $env:LOCALAPPDATA = Join-Path $SourceSuccessDir "local-app-data"
    $SourceInstallDir = Join-Path $env:LOCALAPPDATA "Programs\bitbygit"
    $SourceStartingPath = "$SourceInstallDir;$OriginalProcessPath"
    $env:Path = $SourceStartingPath
    $env:BITBYGIT_TEST_USER_PATH = $SourceStartingPath
    $CapturedUserPath = $null
    $FetchedExactTag = $false
    $CheckedOutExactTag = $false
    $env:MOCK_CARGO_FAILURE = "0"
    $ErrorActionPreference = "Continue"
    Push-Location $SourceSuccessDir
    try {
        $StartingLocation = (Get-Location).Path
        . $SourceScript
        if ((Get-Location).Path -ne $StartingLocation) {
            Fail "source block changed the caller working directory"
        }
    } finally {
        Pop-Location
    }
    $SourceInstalledBinary = Join-Path $SourceInstallDir "bitbygit.exe"
    if ($ErrorActionPreference -ne "Continue") { Fail "source block changed the caller error preference" }
    if ($env:Path -ne $SourceStartingPath) { Fail "source block duplicated an existing process PATH entry" }
    if (($env:Path -split ';' | Where-Object { $_ -eq $SourceInstallDir }).Count -ne 1) { Fail "source block duplicated the process PATH entry" }
    if ($null -ne $CapturedUserPath) { Fail "source block duplicated an existing user PATH entry" }
    if (-not $FetchedExactTag -or -not $CheckedOutExactTag) { Fail "source block did not check out the exact tag" }
    if (-not (Test-Path $SourceInstalledBinary)) { Fail "source block did not install bitbygit.exe" }
    $SourceResolvedBinary = (Get-Command bitbygit -CommandType Application).Source
    if ($SourceResolvedBinary -ne $SourceInstalledBinary) { Fail "source block did not resolve the installed command through PATH" }
    $SourceOutput = bitbygit --version
    if ($LASTEXITCODE -ne 0 -or $SourceOutput -ne "bitbygit $Version") {
        Fail "source block installed the wrong version"
    }

    $SourceFailureDir = Join-Path $Temp "source-failure"
    New-Item -ItemType Directory -Path $SourceFailureDir | Out-Null
    $SourceFailureScript = Join-Path $SourceFailureDir "install-from-source.ps1"
    Set-Content -Path $SourceFailureScript -Value $SourceBlock
    $env:LOCALAPPDATA = Join-Path $SourceFailureDir "local-app-data"
    $env:Path = $OriginalProcessPath
    $FetchedExactTag = $false
    $CheckedOutExactTag = $false
    $env:MOCK_CARGO_FAILURE = "1"
    $ErrorActionPreference = "Continue"
    $SourceBuildFailed = $false
    Push-Location $SourceFailureDir
    try {
        $StartingLocation = (Get-Location).Path
        try {
            . $SourceFailureScript
        } catch {
            $SourceBuildFailed = $true
        }
        if ((Get-Location).Path -ne $StartingLocation) {
            Fail "failed source block changed the caller working directory"
        }
    } finally {
        Pop-Location
    }
    if (-not $SourceBuildFailed) { Fail "source block continued after a failed build" }
    if ($ErrorActionPreference -ne "Continue") { Fail "failed source block changed the caller error preference" }
    if ($env:Path -ne $OriginalProcessPath) { Fail "failed source block changed the process PATH" }

    $global:LASTEXITCODE = 0
    Write-Output "installation docs validation passed on Windows"
} finally {
    $env:LOCALAPPDATA = $OriginalLocalAppData
    $env:Path = $OriginalProcessPath
    Remove-Item -Recurse -Force $Temp -ErrorAction SilentlyContinue
}
