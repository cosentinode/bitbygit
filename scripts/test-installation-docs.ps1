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
    $GetMachinePath = 'GetEnvironmentVariable("Path", "Machine")'
    $SetUserPath = '[Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")'
    if (-not $Block.Contains($GetUserPath) -or -not $Block.Contains($GetMachinePath) -or
        -not $Block.Contains($SetUserPath)) {
        Fail "Windows block does not contain the expected PATH operations"
    }

    $Isolated = $Block.Replace($GetUserPath, 'GetEnvironmentVariable("BITBYGIT_TEST_USER_PATH", "Process")')
    $Isolated = $Isolated.Replace($GetMachinePath, 'GetEnvironmentVariable("BITBYGIT_TEST_MACHINE_PATH", "Process")')
    $Isolated = $Isolated.Replace($SetUserPath, '$script:CapturedUserPath = $NewUserPath')
    if ($Isolated.Contains('"User"') -or $Isolated.Contains('"Machine"')) {
        Fail "Windows block still accesses persistent PATH state"
    }
    return $Isolated
}

function ConvertTo-SelectedVersionBlock([string] $Block, [string] $Heading) {
    $DocumentedAssignment = '$Version = "{0}"' -f $script:WorkspaceVersion
    if (-not $Block.Contains($DocumentedAssignment)) {
        Fail "missing selected version in $Heading powershell block"
    }

    $SelectedAssignment = '$Version = "{0}"' -f $script:SelectedVersion
    return $Block.Replace($DocumentedAssignment, $SelectedAssignment)
}

$DocsText = Get-Content -Raw $DocsPath
$CargoText = Get-Content -Raw $CargoPath
$VersionMatch = [regex]::Match($CargoText, '(?ms)^\[workspace\.package\]\r?\n.*?^version = "([^"]+)"')
if (-not $VersionMatch.Success) { Fail "workspace version was not found" }
$WorkspaceVersion = $VersionMatch.Groups[1].Value
$SelectedVersion = "2.3.4"
if ($SelectedVersion -eq $WorkspaceVersion -or $SelectedVersion -eq "0.1.0") {
    Fail "selected validator version must differ from the documented example"
}
if (-not $DocsText.Contains("`$Version = `"$WorkspaceVersion`"")) {
    Fail "PowerShell examples do not use workspace version $WorkspaceVersion"
}
$UserPathOrder = '$NewUserPath = (@($InstallDir) + $UserPathEntries) -join ";"'
if ([regex]::Matches($DocsText, [regex]::Escape($UserPathOrder)).Count -ne 2) {
    Fail "Windows examples do not prioritize the install directory in user PATH"
}
$ProcessPathOrder = '$env:Path = (@($InstallDir) + $ProcessPathEntries) -join ";"'
if ([regex]::Matches($DocsText, [regex]::Escape($ProcessPathOrder)).Count -ne 2) {
    Fail "Windows examples do not prioritize the install directory in process PATH"
}
$PathEntryFilter = '-not [string]::IsNullOrEmpty($_) -and $_ -ne $InstallDir'
if ([regex]::Matches($DocsText, [regex]::Escape($PathEntryFilter)).Count -ne 4) {
    Fail "Windows examples do not remove empty and duplicate install directory PATH entries"
}
$MachinePathLookup = 'GetEnvironmentVariable("Path", "Machine")'
if ([regex]::Matches($DocsText, [regex]::Escape($MachinePathLookup)).Count -ne 2 -or
    [regex]::Matches($DocsText, [regex]::Escape('User PATH cannot override it in new shells')).Count -ne 2) {
    Fail "Windows examples do not reject machine-level PATH shadowing"
}
$PathExtLookup = '$env:PATHEXT -split ";"'
$CaseInsensitivePathExtensions = '$PathExtensions = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)'
$AddPathExtension = '[void] $PathExtensions.Add($Extension)'
if ([regex]::Matches($DocsText, [regex]::Escape($PathExtLookup)).Count -ne 2 -or
    [regex]::Matches($DocsText, [regex]::Escape($CaseInsensitivePathExtensions)).Count -ne 2 -or
    [regex]::Matches($DocsText, [regex]::Escape($AddPathExtension)).Count -ne 2) {
    Fail "Windows examples do not inspect every PATHEXT application candidate case-insensitively"
}
if ([regex]::Matches($DocsText, '(?m)^& \$ResolvedPath --version\r?$').Count -ne 2) {
    Fail "Windows examples do not finish with PATH-resolved version output"
}
$ApplicationResolution = 'Get-Command bitbygit -CommandType Application -ErrorAction Stop'
if ([regex]::Matches($DocsText, [regex]::Escape($ApplicationResolution)).Count -ne 2 -or
    [regex]::Matches($DocsText, [regex]::Escape('[System.StringComparison]::OrdinalIgnoreCase')).Count -ne 2) {
    Fail "Windows examples do not bypass aliases and functions or verify resolved path identity"
}
$RandomTempName = '[System.IO.Path]::GetRandomFileName()'
$CleanupWarning = '[Console]::Error.WriteLine("Warning: failed to remove temporary directory'
if ([regex]::Matches($DocsText, [regex]::Escape($RandomTempName)).Count -ne 2 -or
    [regex]::Matches($DocsText, 'finally \{').Count -ne 2 -or
    [regex]::Matches($DocsText, [regex]::Escape('Remove-Item -LiteralPath $WorkDir -Recurse -Force')).Count -ne 2 -or
    [regex]::Matches($DocsText, [regex]::Escape($CleanupWarning)).Count -ne 2) {
    Fail "Windows examples do not securely stage and clean up temporary work"
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

$InstallBlock = Get-Block "Windows x86-64 archive" "powershell"
$InstallBlock = ConvertTo-SelectedVersionBlock $InstallBlock "Windows x86-64 archive"
$InstallBlock = ConvertTo-IsolatedPathBlock $InstallBlock
if (-not $InstallBlock.Contains('$Package = "bitbygit-$Version-$Target"')) {
    Fail "Windows package directory does not match the release layout"
}
if (-not $InstallBlock.Contains('& $InstalledBinary --version')) {
    Fail "Windows installation does not validate the installed path"
}
$SourceBlock = Get-Block "Build from source" "powershell"
$SourceBlock = ConvertTo-SelectedVersionBlock $SourceBlock "Build from source"
$SourceBlock = ConvertTo-IsolatedPathBlock $SourceBlock
if (-not $SourceBlock.Contains('git -C $SourceDir fetch --depth 1 https://github.com/cosentinode/bitbygit.git "${Tag}:${Tag}"') -or
    -not $SourceBlock.Contains('git -C $SourceDir checkout --detach $TagCommit') -or
    -not $SourceBlock.Contains('$HeadCommit -ne $TagCommit')) {
    Fail "PowerShell source build is not pinned to the selected tag"
}

New-Item -ItemType Directory -Path $Temp | Out-Null
$OriginalLocalAppData = $env:LOCALAPPDATA
$OriginalProcessPath = $env:Path
$OriginalTemp = $env:TEMP
$OriginalTmp = $env:TMP
$OriginalPathExt = $env:PATHEXT
$OriginalCargoTargetDir = $env:CARGO_TARGET_DIR
$OriginalCargoBuildTarget = $env:CARGO_BUILD_TARGET

try {
    cargo build --locked --release -p bitbygit
    if ($LASTEXITCODE -ne 0) { Fail "cargo build failed" }

    $WorkspaceBinary = Join-Path $Root "target/release/bitbygit.exe"
    $WorkspaceOutput = & $WorkspaceBinary --version
    if ($LASTEXITCODE -ne 0 -or $WorkspaceOutput -ne "bitbygit $WorkspaceVersion") {
        Fail "workspace binary reported the wrong version"
    }

    $FixtureSource = Join-Path $Temp "selected-version-fixture.rs"
    $FixtureBinary = Join-Path $Temp "selected-version-bitbygit.exe"
    $FixtureSourceText = 'fn main() {{ println!("bitbygit {0}"); }}' -f $SelectedVersion
    Set-Content -Path $FixtureSource -Value $FixtureSourceText
    rustc --crate-name selected_version_fixture $FixtureSource -o $FixtureBinary
    if ($LASTEXITCODE -ne 0) { Fail "selected-version fixture build failed" }
    $CustomPathExtensions = @(".VbS", ".BiTbYgIt-Test")
    $env:PATHEXT = (@(".CoM", ".EXE", ".BaT", ".CMD") + $CustomPathExtensions + @(".vbs", "", " ")) -join ";"

    $Target = "x86_64-pc-windows-msvc"
    $Archive = "bitbygit-$SelectedVersion-$Target.zip"
    $Package = "bitbygit-$SelectedVersion-$Target"
    $Assets = Join-Path $Temp "assets"
    $PackageDir = Join-Path $Assets $Package
    $MachinePathDir = Join-Path $Temp "machine-bin"
    New-Item -ItemType Directory -Path $PackageDir | Out-Null
    New-Item -ItemType Directory -Path $MachinePathDir | Out-Null
    Copy-Item $FixtureBinary (Join-Path $PackageDir "bitbygit.exe")
    Compress-Archive -Path $PackageDir -DestinationPath (Join-Path $Assets $Archive)
    $Digest = (Get-FileHash -Algorithm SHA256 (Join-Path $Assets $Archive)).Hash
    Set-Content -Path (Join-Path $Assets "SHA256SUMS") -Value "$Digest  $Archive"

    $SuccessDir = Join-Path $Temp "success"
    $SuccessTemp = Join-Path $SuccessDir "temp"
    $ProtectedPackage = Join-Path $SuccessDir "protected-package"
    New-Item -ItemType Directory -Path $SuccessDir, $SuccessTemp, $ProtectedPackage | Out-Null
    $ProtectedFile = Join-Path $SuccessDir "protected-file"
    Set-Content -Path $ProtectedFile -Value "protected"
    Set-Content -Path (Join-Path $ProtectedPackage "marker") -Value "protected"
    New-Item -ItemType SymbolicLink -Path (Join-Path $SuccessDir $Archive) -Target $ProtectedFile | Out-Null
    New-Item -ItemType SymbolicLink -Path (Join-Path $SuccessDir "SHA256SUMS") -Target $ProtectedFile | Out-Null
    New-Item -ItemType SymbolicLink -Path (Join-Path $SuccessDir $Package) -Target $ProtectedPackage | Out-Null
    $SuccessScript = Join-Path $SuccessDir "install.ps1"
    Set-Content -Path $SuccessScript -Value $InstallBlock
    $env:LOCALAPPDATA = Join-Path $SuccessDir "local-app-data"
    $env:TEMP = $SuccessTemp
    $env:TMP = $SuccessTemp
    $env:Path = $null
    Remove-Item Env:BITBYGIT_TEST_USER_PATH -ErrorAction SilentlyContinue
    $env:BITBYGIT_TEST_MACHINE_PATH = $MachinePathDir
    $CapturedUserPath = $null
    $env:MOCK_DOWNLOAD_DIR = $Assets
    $ErrorActionPreference = "Continue"
    $ShadowInvoked = $false

    function bitbygit {
        $script:ShadowInvoked = $true
        return "bitbygit $SelectedVersion"
    }

    function Invoke-ShadowedBitByGit {
        $script:ShadowInvoked = $true
        return "bitbygit $SelectedVersion"
    }

    Set-Alias -Name bitbygit -Value Invoke-ShadowedBitByGit

    function Invoke-WebRequest {
        param($Uri, $OutFile)
        Copy-Item (Join-Path $env:MOCK_DOWNLOAD_DIR ([System.IO.Path]::GetFileName([uri] $Uri))) $OutFile
    }

    Push-Location $SuccessDir
    try {
        $StartingLocation = (Get-Location).Path
        foreach ($Attempt in 1..2) {
            $ShadowInvoked = $false
            . $SuccessScript
            if ($ShadowInvoked) { Fail "archive block invoked a shadowing alias or function" }
            if ($Attempt -eq 1) { Remove-Item Alias:bitbygit }
            if ((Get-Location).Path -ne $StartingLocation) {
                Fail "archive block changed the caller working directory"
            }
            if ((Get-ChildItem -Force $SuccessTemp).Count -ne 0) {
                Fail "archive block left temporary installation files behind"
            }
        }
    } finally {
        Pop-Location
    }
    Remove-Item Function:bitbygit
    Remove-Item Function:Invoke-ShadowedBitByGit

    if ((Get-Item -Force (Join-Path $SuccessDir $Archive)).LinkType -ne "SymbolicLink" -or
        (Get-Item -Force (Join-Path $SuccessDir "SHA256SUMS")).LinkType -ne "SymbolicLink" -or
        (Get-Item -Force (Join-Path $SuccessDir $Package)).LinkType -ne "SymbolicLink") {
        Fail "archive block replaced a pre-existing working-directory symlink"
    }
    if ((Get-Content $ProtectedFile) -ne "protected" -or
        (Get-Content (Join-Path $ProtectedPackage "marker")) -ne "protected") {
        Fail "archive block modified a pre-existing working-directory symlink target"
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
    if ($LASTEXITCODE -ne 0 -or $Output -ne "bitbygit $SelectedVersion") {
        Fail "archive block installed the wrong version"
    }
    $env:Path = "$($env:BITBYGIT_TEST_MACHINE_PATH);$CapturedUserPath"
    $NewShellResolvedBinary = (Get-Command bitbygit -CommandType Application).Source
    if ($NewShellResolvedBinary -ne $InstalledBinary) { Fail "archive new shell PATH did not resolve the installed command" }
    $NewShellOutput = bitbygit --version
    if ($LASTEXITCODE -ne 0 -or $NewShellOutput -ne "bitbygit $SelectedVersion") {
        Fail "archive new shell PATH resolved the wrong version"
    }

    $MachineConflictDir = Join-Path $Temp "machine-conflict"
    $MachineOldBinaryDir = Join-Path $MachineConflictDir "machine-bin"
    New-Item -ItemType Directory -Path $MachineOldBinaryDir -Force | Out-Null
    Copy-Item $WorkspaceBinary (Join-Path $MachineOldBinaryDir "bitbygit.exe")
    $MachineConflictScript = Join-Path $MachineConflictDir "install.ps1"
    Set-Content -Path $MachineConflictScript -Value $InstallBlock
    $MachineConflictTemp = Join-Path $MachineConflictDir "temp"
    New-Item -ItemType Directory -Path $MachineConflictTemp | Out-Null
    $env:LOCALAPPDATA = Join-Path $MachineConflictDir "local-app-data"
    $env:TEMP = $MachineConflictTemp
    $env:TMP = $MachineConflictTemp
    $env:Path = "$MachineOldBinaryDir;$OriginalProcessPath"
    $env:BITBYGIT_TEST_MACHINE_PATH = $MachineOldBinaryDir
    Remove-Item Env:BITBYGIT_TEST_USER_PATH -ErrorAction SilentlyContinue
    $CapturedUserPath = $null
    $env:MOCK_DOWNLOAD_DIR = $Assets
    $MachineConflictStartingPath = $env:Path
    $MachineConflictStartingPathExt = $env:PATHEXT
    $OldMachineOutput = bitbygit --version
    if ($LASTEXITCODE -ne 0 -or $OldMachineOutput -ne "bitbygit $WorkspaceVersion") {
        Fail "older machine-level bitbygit.exe fixture reported the wrong version"
    }
    foreach ($Extension in (@(".exe", ".com", ".bat", ".cmd") + $CustomPathExtensions)) {
        Remove-Item (Join-Path $MachineOldBinaryDir "bitbygit.*") -Force
        Copy-Item $WorkspaceBinary (Join-Path $MachineOldBinaryDir "bitbygit$Extension")
        $MachineConflictFailed = $false
        Push-Location $MachineConflictDir
        try {
            try {
                . $MachineConflictScript
            } catch {
                $MachineConflictFailed = $_.Exception.Message.Contains("User PATH cannot override it in new shells")
            }
        } finally {
            Pop-Location
        }
        if (-not $MachineConflictFailed) { Fail "archive block did not reject machine-level $Extension PATH shadowing" }
        if ((Get-ChildItem -Force $MachineConflictTemp).Count -ne 0) { Fail "failed archive block left temporary installation files behind" }
        if ($null -ne $CapturedUserPath) { Fail "machine-level conflict changed persistent user PATH" }
        if ($env:Path -ne $MachineConflictStartingPath) { Fail "machine-level conflict changed process PATH" }
        if ($env:PATHEXT -ne $MachineConflictStartingPathExt) { Fail "machine-level conflict changed PATHEXT" }
        $ConflictInstalledBinary = Join-Path $env:LOCALAPPDATA "Programs\bitbygit\bitbygit.exe"
        $ConflictOutput = & $ConflictInstalledBinary --version
        if ($LASTEXITCODE -ne 0 -or $ConflictOutput -ne "bitbygit $SelectedVersion") {
            Fail "machine-level conflict did not leave the selected binary available by explicit path"
        }
    }
    $env:BITBYGIT_TEST_MACHINE_PATH = $MachinePathDir
    Push-Location $MachineConflictDir
    try {
        $StartingLocation = (Get-Location).Path
        . $MachineConflictScript
        if ((Get-Location).Path -ne $StartingLocation) { Fail "retried archive block changed the caller working directory" }
    } finally {
        Pop-Location
    }
    if ((Get-ChildItem -Force $MachineConflictTemp).Count -ne 0) { Fail "retried archive block left temporary installation files behind" }
    $ConflictRetryOutput = bitbygit --version
    if ($LASTEXITCODE -ne 0 -or $ConflictRetryOutput -ne "bitbygit $SelectedVersion") {
        Fail "archive block could not retry after removing a machine-level conflict"
    }

    $FailureAssets = Join-Path $Temp "failure-assets"
    $FailureDir = Join-Path $Temp "failure"
    $FailureTemp = Join-Path $FailureDir "temp"
    New-Item -ItemType Directory -Path $FailureAssets, $FailureDir, $FailureTemp | Out-Null
    Copy-Item (Join-Path $Assets $Archive) $FailureAssets
    Set-Content -Path (Join-Path $FailureAssets "SHA256SUMS") -Value "$('0' * 64)  $Archive"
    $FailureScript = Join-Path $FailureDir "install.ps1"
    Set-Content -Path $FailureScript -Value $InstallBlock
    $env:LOCALAPPDATA = Join-Path $FailureDir "local-app-data"
    $env:TEMP = $FailureTemp
    $env:TMP = $FailureTemp
    $env:Path = $OriginalProcessPath
    $env:BITBYGIT_TEST_MACHINE_PATH = $MachinePathDir
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
    if ((Get-ChildItem -Force $FailureTemp).Count -ne 0) { Fail "failed archive block left temporary installation files behind" }
    if ($ErrorActionPreference -ne "Continue") { Fail "failed archive block changed the caller error preference" }
    if ($env:Path -ne $OriginalProcessPath) { Fail "failed archive block changed the process PATH" }

    $CleanupFailureBlock = $InstallBlock.Replace(
        '        Remove-Item -LiteralPath $WorkDir -Recurse -Force',
        '        throw "forced cleanup failure"'
    )
    if ($CleanupFailureBlock -eq $InstallBlock) { Fail "could not force an archive cleanup failure" }
    $CleanupFailureScript = Join-Path $FailureDir "install-cleanup-failure.ps1"
    Set-Content -Path $CleanupFailureScript -Value $CleanupFailureBlock
    $CleanupErrorWriter = [System.IO.StringWriter]::new()
    $OriginalErrorWriter = [Console]::Error
    $CleanupPrimaryMessage = $null
    [Console]::SetError($CleanupErrorWriter)
    Push-Location $FailureDir
    try {
        try {
            . $CleanupFailureScript
        } catch {
            $CleanupPrimaryMessage = $_.Exception.Message
        }
    } finally {
        Pop-Location
        [Console]::SetError($OriginalErrorWriter)
    }
    if ([string]::IsNullOrEmpty($CleanupPrimaryMessage) -or
        -not $CleanupPrimaryMessage.Contains("Checksum verification failed")) {
        Fail "archive cleanup failure masked the checksum failure: $CleanupPrimaryMessage"
    }
    if (-not $CleanupErrorWriter.ToString().Contains("forced cleanup failure")) {
        Fail "archive block did not report the cleanup failure"
    }

    function git {
        param([Parameter(ValueFromRemainingArguments = $true)] [string[]] $Arguments)

        $ExpectedTag = "refs/tags/v$SelectedVersion"
        if ($Arguments.Count -eq 3 -and $Arguments[0] -eq "-C" -and
            $Arguments[2] -eq "init") {
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
        param(
            [Alias("p")] [string] $Package,
            [Parameter(ValueFromRemainingArguments = $true)] [object[]] $Arguments
        )

        if ($env:MOCK_CARGO_FAILURE -eq "1") {
            $global:LASTEXITCODE = 1
            return
        }

        $ManifestIndex = [array]::IndexOf($Arguments, "--manifest-path")
        $TargetDirIndex = [array]::IndexOf($Arguments, "--target-dir")
        $TargetIndex = [array]::IndexOf($Arguments, "--target")
        if ($Arguments[0] -ne "build" -or $ManifestIndex -lt 0 -or $TargetDirIndex -lt 0 -or
            $TargetIndex -lt 0 -or
            -not $Arguments.Contains("--locked") -or -not $Arguments.Contains("--release") -or
            $Package -ne "bitbygit") {
            throw "unexpected cargo arguments: $Arguments"
        }
        $ManifestPath = [string] $Arguments[$ManifestIndex + 1]
        $TargetDir = [string] $Arguments[$TargetDirIndex + 1]
        $Target = [string] $Arguments[$TargetIndex + 1]
        $ExpectedTargetDir = Join-Path (Split-Path -Parent (Split-Path -Parent $ManifestPath)) "cargo-target"
        if ($TargetDir -ne $ExpectedTargetDir -or $TargetDir -eq $env:CARGO_TARGET_DIR -or
            $Target -ne $script:HostTarget) {
            throw "cargo output was not isolated for the native target: $Arguments"
        }
        $BuildDir = Join-Path $TargetDir "$Target\release"
        New-Item -ItemType Directory -Path $BuildDir | Out-Null
        Copy-Item $FixtureBinary (Join-Path $BuildDir "bitbygit.exe")
        $script:CargoUsedIsolatedTarget = $true
        $global:LASTEXITCODE = 0
    }

    $SourceSuccessDir = Join-Path $Temp "source-success"
    $SourceSuccessTemp = Join-Path $SourceSuccessDir "temp"
    $ProtectedSource = Join-Path $SourceSuccessDir "protected-source"
    New-Item -ItemType Directory -Path $SourceSuccessDir, $SourceSuccessTemp, $ProtectedSource | Out-Null
    Set-Content -Path (Join-Path $ProtectedSource "marker") -Value "protected"
    New-Item -ItemType SymbolicLink -Path (Join-Path $SourceSuccessDir "bitbygit") -Target $ProtectedSource | Out-Null
    $SourceScript = Join-Path $SourceSuccessDir "install-from-source.ps1"
    Set-Content -Path $SourceScript -Value $SourceBlock
    $env:LOCALAPPDATA = Join-Path $SourceSuccessDir "local-app-data"
    $env:TEMP = $SourceSuccessTemp
    $env:TMP = $SourceSuccessTemp
    $SourceInstallDir = Join-Path $env:LOCALAPPDATA "Programs\bitbygit"
    $OldBinaryDir = Join-Path $SourceSuccessDir "old-bin"
    New-Item -ItemType Directory -Path $OldBinaryDir | Out-Null
    Copy-Item $WorkspaceBinary (Join-Path $OldBinaryDir "bitbygit.exe")
    $SourceUserPath = "$OldBinaryDir;$SourceInstallDir;$SourceInstallDir"
    $SourceStartingPath = "$MachinePathDir;$SourceUserPath"
    $SourceExpectedPath = "$SourceInstallDir;$MachinePathDir;$OldBinaryDir"
    $SourceExpectedUserPath = "$SourceInstallDir;$OldBinaryDir"
    $env:Path = $SourceStartingPath
    $env:BITBYGIT_TEST_MACHINE_PATH = $MachinePathDir
    $env:BITBYGIT_TEST_USER_PATH = $SourceUserPath
    $env:CARGO_TARGET_DIR = Join-Path $SourceSuccessDir "configured-target"
    $env:CARGO_BUILD_TARGET = "configured-non-host-target"
    $HostLine = rustc -vV | Where-Object { $_.StartsWith("host: ") } | Select-Object -First 1
    if (-not $HostLine) { Fail "could not determine validator host target" }
    $HostTarget = $HostLine.Substring(6)
    $OldResolvedBinary = (Get-Command bitbygit -CommandType Application | Select-Object -First 1).Source
    if ($OldResolvedBinary -ne (Join-Path $OldBinaryDir "bitbygit.exe")) { Fail "older bitbygit.exe fixture was not first on PATH" }
    $OldOutput = bitbygit --version
    if ($LASTEXITCODE -ne 0 -or $OldOutput -ne "bitbygit $WorkspaceVersion") {
        Fail "older bitbygit.exe fixture reported the wrong version"
    }
    $CapturedUserPath = $null
    $FetchedExactTag = $false
    $CheckedOutExactTag = $false
    $env:MOCK_CARGO_FAILURE = "0"
    $ErrorActionPreference = "Continue"
    $ShadowInvoked = $false

    function bitbygit {
        $script:ShadowInvoked = $true
        return "bitbygit $SelectedVersion"
    }

    function Invoke-ShadowedBitByGit {
        $script:ShadowInvoked = $true
        return "bitbygit $SelectedVersion"
    }

    Set-Alias -Name bitbygit -Value Invoke-ShadowedBitByGit
    Push-Location $SourceSuccessDir
    try {
        $StartingLocation = (Get-Location).Path
        foreach ($Attempt in 1..2) {
            $FetchedExactTag = $false
            $CheckedOutExactTag = $false
            $CargoUsedIsolatedTarget = $false
            $ShadowInvoked = $false
            . $SourceScript
            if ($ShadowInvoked) { Fail "source block invoked a shadowing alias or function" }
            if ($Attempt -eq 1) { Remove-Item Alias:bitbygit }
            if ((Get-Location).Path -ne $StartingLocation) {
                Fail "source block changed the caller working directory"
            }
            if ((Get-ChildItem -Force $SourceSuccessTemp).Count -ne 0) {
                Fail "source block left temporary build files behind"
            }
            if (-not $CargoUsedIsolatedTarget) { Fail "source block did not use isolated native Cargo output" }
        }
    } finally {
        Pop-Location
    }
    Remove-Item Function:bitbygit
    Remove-Item Function:Invoke-ShadowedBitByGit
    if ((Get-Item -Force (Join-Path $SourceSuccessDir "bitbygit")).LinkType -ne "SymbolicLink" -or
        (Get-Content (Join-Path $ProtectedSource "marker")) -ne "protected") {
        Fail "source block modified a pre-existing working-directory source symlink"
    }
    $SourceInstalledBinary = Join-Path $SourceInstallDir "bitbygit.exe"
    if ($ErrorActionPreference -ne "Continue") { Fail "source block changed the caller error preference" }
    if ($env:Path -ne $SourceExpectedPath) { Fail "source block did not prioritize and deduplicate the process PATH entry" }
    if (($env:Path -split ';' | Where-Object { $_ -eq $SourceInstallDir }).Count -ne 1) { Fail "source block duplicated the process PATH entry" }
    if ($CapturedUserPath -ne $SourceExpectedUserPath) { Fail "source block did not prioritize and deduplicate the user PATH entry" }
    if (($CapturedUserPath -split ';' | Where-Object { $_ -eq $SourceInstallDir }).Count -ne 1) { Fail "source block duplicated the user PATH entry" }
    if (-not $FetchedExactTag -or -not $CheckedOutExactTag) { Fail "source block did not check out the exact tag" }
    if (-not (Test-Path $SourceInstalledBinary)) { Fail "source block did not install bitbygit.exe" }
    $SourceResolvedBinary = (Get-Command bitbygit -CommandType Application | Select-Object -First 1).Source
    if ($SourceResolvedBinary -ne $SourceInstalledBinary) { Fail "source block did not resolve the installed command through PATH" }
    $SourceOutput = bitbygit --version
    if ($LASTEXITCODE -ne 0 -or $SourceOutput -ne "bitbygit $SelectedVersion") {
        Fail "source block installed the wrong version"
    }
    $env:Path = "$($env:BITBYGIT_TEST_MACHINE_PATH);$CapturedUserPath"
    $NewShellResolvedBinary = (Get-Command bitbygit -CommandType Application | Select-Object -First 1).Source
    if ($NewShellResolvedBinary -ne $SourceInstalledBinary) { Fail "new shell PATH resolved the older bitbygit.exe" }
    $NewShellOutput = bitbygit --version
    if ($LASTEXITCODE -ne 0 -or $NewShellOutput -ne "bitbygit $SelectedVersion") {
        Fail "new shell PATH resolved the wrong bitbygit version"
    }

    $SourceFailureDir = Join-Path $Temp "source-failure"
    $SourceFailureTemp = Join-Path $SourceFailureDir "temp"
    New-Item -ItemType Directory -Path $SourceFailureDir, $SourceFailureTemp | Out-Null
    $SourceFailureScript = Join-Path $SourceFailureDir "install-from-source.ps1"
    Set-Content -Path $SourceFailureScript -Value $SourceBlock
    $env:LOCALAPPDATA = Join-Path $SourceFailureDir "local-app-data"
    $env:TEMP = $SourceFailureTemp
    $env:TMP = $SourceFailureTemp
    $env:Path = $OriginalProcessPath
    $env:BITBYGIT_TEST_MACHINE_PATH = $MachinePathDir
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
    if ((Get-ChildItem -Force $SourceFailureTemp).Count -ne 0) { Fail "failed source block left temporary build files behind" }
    if ($ErrorActionPreference -ne "Continue") { Fail "failed source block changed the caller error preference" }
    if ($env:Path -ne $OriginalProcessPath) { Fail "failed source block changed the process PATH" }

    $env:MOCK_CARGO_FAILURE = "0"
    $CapturedUserPath = $null
    Remove-Item Env:BITBYGIT_TEST_USER_PATH -ErrorAction SilentlyContinue
    Push-Location $SourceFailureDir
    try {
        $StartingLocation = (Get-Location).Path
        . $SourceFailureScript
        if ((Get-Location).Path -ne $StartingLocation) { Fail "retried source block changed the caller working directory" }
    } finally {
        Pop-Location
    }
    if ((Get-ChildItem -Force $SourceFailureTemp).Count -ne 0) { Fail "retried source block left temporary build files behind" }
    $SourceRetryOutput = bitbygit --version
    if ($LASTEXITCODE -ne 0 -or $SourceRetryOutput -ne "bitbygit $SelectedVersion") {
        Fail "source block could not retry after a failed build"
    }

    $global:LASTEXITCODE = 0
    Write-Output "installation docs validation passed on Windows"
} finally {
    $env:LOCALAPPDATA = $OriginalLocalAppData
    $env:Path = $OriginalProcessPath
    $env:TEMP = $OriginalTemp
    $env:TMP = $OriginalTmp
    $env:PATHEXT = $OriginalPathExt
    $env:CARGO_TARGET_DIR = $OriginalCargoTargetDir
    $env:CARGO_BUILD_TARGET = $OriginalCargoBuildTarget
    Remove-Item -Recurse -Force $Temp -ErrorAction SilentlyContinue
}
