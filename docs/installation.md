# Installation

`bitbygit` can be installed from a GitHub Release archive or built from a
published source tag. No tagged release has been published yet. Use these
instructions once the version you want appears on the
[Releases page](https://github.com/cosentinode/bitbygit/releases).

## Runtime prerequisites

- `git` must be installed and available on `PATH`.
- [GitHub CLI (`gh`)](https://cli.github.com/) is optional. It is required only
  for GitHub-specific operations such as opening a pull request; run
  `gh auth login` before using those operations.
- Rust and a native build toolchain are not required when using a release
  archive. See [Build from source](#build-from-source) for source prerequisites.

## Supported release targets

| Platform | Target | Archive |
| --- | --- | --- |
| Linux x86-64 | `x86_64-unknown-linux-gnu` | `bitbygit-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| macOS Intel | `x86_64-apple-darwin` | `bitbygit-<version>-x86_64-apple-darwin.tar.gz` |
| macOS Apple silicon | `aarch64-apple-darwin` | `bitbygit-<version>-aarch64-apple-darwin.tar.gz` |
| Windows x86-64 | `x86_64-pc-windows-msvc` | `bitbygit-<version>-x86_64-pc-windows-msvc.zip` |

The Linux artifact uses GNU libc and is checked not to require a GLIBC symbol
newer than 2.35. The release workflow does not produce artifacts for other
architectures or operating systems.

Each tagged release also includes `SHA256SUMS`. Verify the downloaded archive
before extracting or running it. The examples below use `0.1.0`; set `VERSION`
or `$Version` to an available release version without the leading `v`.

## Linux x86-64 archive

Run these commands in Bash. They require `curl`, `tar`, `awk`, and GNU
coreutils (`sha256sum`, `mkdir`, and `install`) and stop before extraction or
installation if any download or checksum check fails:

```bash
VERSION=0.1.0
VERSION="${VERSION}" bash -euo pipefail <<'BITBYGIT_INSTALL' &&
TARGET=x86_64-unknown-linux-gnu
ARCHIVE="bitbygit-${VERSION}-${TARGET}.tar.gz"
PACKAGE="bitbygit-${VERSION}-${TARGET}"
BASE_URL="https://github.com/cosentinode/bitbygit/releases/download/v${VERSION}"
WORK_DIR="$(mktemp -d)"
cleanup() {
  status=$?
  trap - EXIT
  if ! rm -rf -- "${WORK_DIR}"; then
    printf 'Warning: failed to remove temporary directory %s\n' "${WORK_DIR}" >&2
  fi
  exit "${status}"
}
trap cleanup EXIT
cd "${WORK_DIR}"

curl -fLO "${BASE_URL}/${ARCHIVE}"
curl -fLO "${BASE_URL}/SHA256SUMS"
awk -v archive="${ARCHIVE}" '$2 == archive && NF == 2 { print; found=1 } END { exit !found }' SHA256SUMS |
  sha256sum --check -
tar -xzf "${ARCHIVE}"

mkdir -p "${HOME}/.local/bin"
install -m 0755 "${PACKAGE}/bitbygit" "${HOME}/.local/bin/bitbygit"
output="$("${HOME}/.local/bin/bitbygit" --version)"
if [[ "${output}" != "bitbygit ${VERSION}" ]]; then
  printf 'Expected bitbygit %s, got %s\n' "${VERSION}" "${output}" >&2
  exit 1
fi
BITBYGIT_INSTALL
{
install_dir="${HOME}/.local/bin"
new_path="${install_dir}"
if [[ -z "${PATH+x}" ]]; then
  PATH="$(command -p getconf PATH)"
fi
remaining_path="${PATH}"
while [[ "${remaining_path}" == *:* ]]; do
  entry="${remaining_path%%:*}"
  remaining_path="${remaining_path#*:}"
  [[ "${entry}" == "${install_dir}" ]] || new_path+=":${entry}"
done
[[ "${remaining_path}" == "${install_dir}" ]] || new_path+=":${remaining_path}"
export PATH="${new_path}"
resolved_binary="$(type -P bitbygit || true)"
if [[ -n "${resolved_binary}" && "${resolved_binary}" -ef "${install_dir}/bitbygit" ]] &&
  path_output="$(command bitbygit --version)" && [[ "${path_output}" == "bitbygit ${VERSION}" ]]; then
  command bitbygit --version
else
  printf 'Expected %s to resolve as bitbygit %s, got %s (%s)\n' \
    "${install_dir}/bitbygit" "${VERSION}" "${resolved_binary:-no application}" "${path_output:-no output}" >&2
  false
fi
}
```

Add `export PATH="$HOME/.local/bin:$PATH"` to your shell startup file to keep
the command available in new shells.

## macOS archive

Run these commands in Bash. They select the Intel or Apple silicon artifact
automatically and require `curl`, `awk`, `shasum`, and `tar`, which are included with
macOS. The shell stops before extraction or installation if any download or
checksum check fails:

```bash
VERSION=0.1.0
VERSION="${VERSION}" bash -euo pipefail <<'BITBYGIT_INSTALL' &&
case "$(uname -m)" in
  x86_64) TARGET=x86_64-apple-darwin ;;
  arm64) TARGET=aarch64-apple-darwin ;;
  *) echo "Unsupported macOS architecture: $(uname -m)" >&2; exit 1 ;;
esac
ARCHIVE="bitbygit-${VERSION}-${TARGET}.tar.gz"
PACKAGE="bitbygit-${VERSION}-${TARGET}"
BASE_URL="https://github.com/cosentinode/bitbygit/releases/download/v${VERSION}"
WORK_DIR="$(mktemp -d)"
cleanup() {
  status=$?
  trap - EXIT
  if ! rm -rf -- "${WORK_DIR}"; then
    printf 'Warning: failed to remove temporary directory %s\n' "${WORK_DIR}" >&2
  fi
  exit "${status}"
}
trap cleanup EXIT
cd "${WORK_DIR}"

curl -fLO "${BASE_URL}/${ARCHIVE}"
curl -fLO "${BASE_URL}/SHA256SUMS"
awk -v archive="${ARCHIVE}" '$2 == archive && NF == 2 { print; found=1 } END { exit !found }' SHA256SUMS |
  shasum -a 256 --check -
tar -xzf "${ARCHIVE}"

mkdir -p "${HOME}/.local/bin"
install -m 0755 "${PACKAGE}/bitbygit" "${HOME}/.local/bin/bitbygit"
output="$("${HOME}/.local/bin/bitbygit" --version)"
if [[ "${output}" != "bitbygit ${VERSION}" ]]; then
  printf 'Expected bitbygit %s, got %s\n' "${VERSION}" "${output}" >&2
  exit 1
fi
BITBYGIT_INSTALL
{
install_dir="${HOME}/.local/bin"
new_path="${install_dir}"
if [[ -z "${PATH+x}" ]]; then
  PATH="$(command -p getconf PATH)"
fi
remaining_path="${PATH}"
while [[ "${remaining_path}" == *:* ]]; do
  entry="${remaining_path%%:*}"
  remaining_path="${remaining_path#*:}"
  [[ "${entry}" == "${install_dir}" ]] || new_path+=":${entry}"
done
[[ "${remaining_path}" == "${install_dir}" ]] || new_path+=":${remaining_path}"
export PATH="${new_path}"
resolved_binary="$(type -P bitbygit || true)"
if [[ -n "${resolved_binary}" && "${resolved_binary}" -ef "${install_dir}/bitbygit" ]] &&
  path_output="$(command bitbygit --version)" && [[ "${path_output}" == "bitbygit ${VERSION}" ]]; then
  command bitbygit --version
else
  printf 'Expected %s to resolve as bitbygit %s, got %s (%s)\n' \
    "${install_dir}/bitbygit" "${VERSION}" "${resolved_binary:-no application}" "${path_output:-no output}" >&2
  false
fi
}
```

Add `export PATH="$HOME/.local/bin:$PATH"` to `~/.zprofile` (or the startup file
for your shell) to keep the command available in new shells.

## Windows x86-64 archive

Run these commands in PowerShell:

```powershell
& {
$ErrorActionPreference = "Stop"

$Version = "0.1.0"
$Target = "x86_64-pc-windows-msvc"
$Archive = "bitbygit-$Version-$Target.zip"
$Package = "bitbygit-$Version-$Target"
$BaseUrl = "https://github.com/cosentinode/bitbygit/releases/download/v$Version"
$WorkDir = (New-Item -ItemType Directory -Path (Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName()))).FullName

try {
$ArchivePath = Join-Path $WorkDir $Archive
$ChecksumsPath = Join-Path $WorkDir "SHA256SUMS"
Invoke-WebRequest -Uri "$BaseUrl/$Archive" -OutFile $ArchivePath
Invoke-WebRequest -Uri "$BaseUrl/SHA256SUMS" -OutFile $ChecksumsPath
$ExpectedLine = Get-Content $ChecksumsPath | Where-Object { $_.EndsWith("  $Archive") }
if (-not $ExpectedLine) { throw "No checksum found for $Archive" }
$Expected = ($ExpectedLine -split '\s+')[0]
$Actual = (Get-FileHash -Algorithm SHA256 $ArchivePath).Hash
if ($Actual -ne $Expected) { throw "Checksum verification failed for $Archive" }
Expand-Archive -Path $ArchivePath -DestinationPath $WorkDir

$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\bitbygit"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item (Join-Path $WorkDir "$Package\bitbygit.exe") $InstallDir
$InstalledBinary = Join-Path $InstallDir "bitbygit.exe"
$Output = & $InstalledBinary --version
if ($LASTEXITCODE -ne 0 -or $Output -ne "bitbygit $Version") {
    throw "Expected bitbygit $Version, got $Output"
}
$PathExtensions = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
$env:PATHEXT -split ";" | ForEach-Object {
    $Extension = $_.Trim()
    if (-not [string]::IsNullOrWhiteSpace($Extension)) {
        [void] $PathExtensions.Add($Extension)
    }
}
$MachinePath = [Environment]::GetEnvironmentVariable("Path", "Machine")
$MachineCommand = $MachinePath -split ";" | ForEach-Object {
    $Entry = [Environment]::ExpandEnvironmentVariables($_.Trim().Trim('"'))
    if (-not [string]::IsNullOrWhiteSpace($Entry)) {
        foreach ($Extension in $PathExtensions) {
            Join-Path $Entry "bitbygit$Extension"
        }
    }
} | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
if ($MachineCommand) {
    throw "Machine PATH already contains $MachineCommand. User PATH cannot override it in new shells. Remove or update that machine-level installation, then rerun; until then invoke $InstalledBinary explicitly."
}
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
$UserPathEntries = @($UserPath -split ";" | Where-Object {
    -not [string]::IsNullOrEmpty($_) -and $_ -ne $InstallDir
})
$NewUserPath = (@($InstallDir) + $UserPathEntries) -join ";"
$ProcessPathEntries = @($env:Path -split ";" | Where-Object {
    -not [string]::IsNullOrEmpty($_) -and $_ -ne $InstallDir
})
$PreviousProcessPath = $env:Path
try {
    $env:Path = (@($InstallDir) + $ProcessPathEntries) -join ";"
    $ResolvedCommand = Get-Command bitbygit -CommandType Application -ErrorAction Stop | Select-Object -First 1
    $ResolvedPath = (Resolve-Path -LiteralPath $ResolvedCommand.Source).Path
    $InstalledPath = (Resolve-Path -LiteralPath $InstalledBinary).Path
    if (-not [string]::Equals($ResolvedPath, $InstalledPath, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Expected $InstalledPath on PATH, got $ResolvedPath"
    }
    $PathOutput = & $ResolvedPath --version
    if ($LASTEXITCODE -ne 0 -or $PathOutput -ne "bitbygit $Version") {
        throw "Expected bitbygit $Version on PATH, got $PathOutput"
    }
    [Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")
} catch {
    $env:Path = $PreviousProcessPath
    throw
}
& $ResolvedPath --version
} finally {
    try {
        Remove-Item -LiteralPath $WorkDir -Recurse -Force
    } catch {
        [Console]::Error.WriteLine("Warning: failed to remove temporary directory ${WorkDir}: $($_.Exception.Message)")
    }
}
}
```

Windows places machine `PATH` entries before user entries in new processes. The
block therefore refuses to update user `PATH` when a machine entry already
contains a `bitbygit` application with any non-empty executable extension
enabled by `PATHEXT`, matched case-insensitively; otherwise an older
machine-level installation could silently win after restarting PowerShell.
Remove or update that machine-level installation and rerun the block. The
selected file is still available through the unambiguous explicit invocation:

```powershell
& "$env:LOCALAPPDATA\Programs\bitbygit\bitbygit.exe" --version
```

When no machine-level conflict exists, new shells use the updated user `PATH`.

## Build from source

Source builds require `git`, [Rust](https://www.rust-lang.org/tools/install) 1.85
or newer, Cargo, and the native tools used by the selected Rust target:

- Linux GNU targets require a C compiler, linker, and libc development headers,
  commonly installed through the distribution's `build-essential` or
  equivalent package. The installation commands also use GNU coreutils
  (`mkdir` and `install`).
- macOS requires the Xcode Command Line Tools (`xcode-select --install`).
- `x86_64-pc-windows-msvc` requires Visual Studio 2022 Build Tools with the
  **Desktop development with C++** workload, including MSVC and a Windows SDK.

The following commands build the selected published release tag, not the moving
development branch. Because no tag has been published yet, `v0.1.0` will become
usable only if that release appears on the Releases page.

On Linux or macOS, run:

```bash
VERSION=0.1.0
VERSION="${VERSION}" bash -euo pipefail <<'BITBYGIT_INSTALL' &&
TAG="refs/tags/v${VERSION}"
WORK_DIR="$(mktemp -d)"
SOURCE_DIR="${WORK_DIR}/bitbygit"
TARGET_DIR="${WORK_DIR}/cargo-target"
cleanup() {
  status=$?
  trap - EXIT
  if ! rm -rf -- "${WORK_DIR}"; then
    printf 'Warning: failed to remove temporary directory %s\n' "${WORK_DIR}" >&2
  fi
  exit "${status}"
}
trap cleanup EXIT

mkdir "${SOURCE_DIR}"
git -C "${SOURCE_DIR}" init
git -C "${SOURCE_DIR}" fetch --depth 1 https://github.com/cosentinode/bitbygit.git "${TAG}:${TAG}"
tag_commit="$(git -C "${SOURCE_DIR}" rev-parse --verify "${TAG}^{commit}")"
git -C "${SOURCE_DIR}" checkout --detach "${tag_commit}"
[[ "$(git -C "${SOURCE_DIR}" rev-parse --verify HEAD)" == "${tag_commit}" ]]
rustc_version="$(rustc -vV)"
host_target=
while IFS= read -r rustc_line; do
  case "${rustc_line}" in
    "host: "*) host_target="${rustc_line#host: }"; break ;;
  esac
done <<< "${rustc_version}"
[[ -n "${host_target}" ]]
cargo build --manifest-path "${SOURCE_DIR}/Cargo.toml" --locked --release -p bitbygit \
  --target-dir "${TARGET_DIR}" --target "${host_target}"
built_binary="${TARGET_DIR}/${host_target}/release/bitbygit"
built_output="$("${built_binary}" --version)"
if [[ "${built_output}" != "bitbygit ${VERSION}" ]]; then
  printf 'Expected bitbygit %s, got %s\n' "${VERSION}" "${built_output}" >&2
  exit 1
fi

mkdir -p "${HOME}/.local/bin"
install -m 0755 "${built_binary}" "${HOME}/.local/bin/bitbygit"
installed_output="$("${HOME}/.local/bin/bitbygit" --version)"
if [[ "${installed_output}" != "bitbygit ${VERSION}" ]]; then
  printf 'Expected bitbygit %s, got %s\n' "${VERSION}" "${installed_output}" >&2
  exit 1
fi
BITBYGIT_INSTALL
{
install_dir="${HOME}/.local/bin"
new_path="${install_dir}"
if [[ -z "${PATH+x}" ]]; then
  PATH="$(command -p getconf PATH)"
fi
remaining_path="${PATH}"
while [[ "${remaining_path}" == *:* ]]; do
  entry="${remaining_path%%:*}"
  remaining_path="${remaining_path#*:}"
  [[ "${entry}" == "${install_dir}" ]] || new_path+=":${entry}"
done
[[ "${remaining_path}" == "${install_dir}" ]] || new_path+=":${remaining_path}"
export PATH="${new_path}"
resolved_binary="$(type -P bitbygit || true)"
if [[ -n "${resolved_binary}" && "${resolved_binary}" -ef "${install_dir}/bitbygit" ]] &&
  path_output="$(command bitbygit --version)" && [[ "${path_output}" == "bitbygit ${VERSION}" ]]; then
  command bitbygit --version
else
  printf 'Expected %s to resolve as bitbygit %s, got %s (%s)\n' \
    "${install_dir}/bitbygit" "${VERSION}" "${resolved_binary:-no application}" "${path_output:-no output}" >&2
  false
fi
}
```

On Windows, run in PowerShell:

```powershell
& {
$ErrorActionPreference = "Stop"

$Version = "0.1.0"
$Tag = "refs/tags/v$Version"
$WorkDir = (New-Item -ItemType Directory -Path (Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName()))).FullName
$SourceDir = Join-Path $WorkDir "bitbygit"
$TargetDir = Join-Path $WorkDir "cargo-target"

try {
New-Item -ItemType Directory -Path $SourceDir | Out-Null
git -C $SourceDir init
if ($LASTEXITCODE -ne 0) { throw "git init failed" }
git -C $SourceDir fetch --depth 1 https://github.com/cosentinode/bitbygit.git "${Tag}:${Tag}"
if ($LASTEXITCODE -ne 0) { throw "git fetch failed" }
$TagCommit = git -C $SourceDir rev-parse --verify "${Tag}^{commit}"
if ($LASTEXITCODE -ne 0) { throw "release tag verification failed" }
git -C $SourceDir checkout --detach $TagCommit
if ($LASTEXITCODE -ne 0) { throw "release tag checkout failed" }
$HeadCommit = git -C $SourceDir rev-parse --verify HEAD
if ($LASTEXITCODE -ne 0 -or $HeadCommit -ne $TagCommit) { throw "release tag checkout verification failed" }
$RustcVersion = rustc -vV
if ($LASTEXITCODE -ne 0) { throw "rustc version detection failed" }
$HostLine = $RustcVersion | Where-Object { $_.StartsWith("host: ") } | Select-Object -First 1
if (-not $HostLine) { throw "rustc host target detection failed" }
$HostTarget = $HostLine.Substring(6)
cargo build --manifest-path (Join-Path $SourceDir "Cargo.toml") --locked --release -p bitbygit --target-dir $TargetDir --target $HostTarget
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
$BuiltBinary = Join-Path $TargetDir "$HostTarget\release\bitbygit.exe"
$BuiltOutput = & $BuiltBinary --version
if ($LASTEXITCODE -ne 0 -or $BuiltOutput -ne "bitbygit $Version") {
    throw "Expected bitbygit $Version, got $BuiltOutput"
}

$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\bitbygit"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item $BuiltBinary $InstallDir
$InstalledBinary = Join-Path $InstallDir "bitbygit.exe"
$InstalledOutput = & $InstalledBinary --version
if ($LASTEXITCODE -ne 0 -or $InstalledOutput -ne "bitbygit $Version") {
    throw "Expected bitbygit $Version, got $InstalledOutput"
}
$PathExtensions = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
$env:PATHEXT -split ";" | ForEach-Object {
    $Extension = $_.Trim()
    if (-not [string]::IsNullOrWhiteSpace($Extension)) {
        [void] $PathExtensions.Add($Extension)
    }
}
$MachinePath = [Environment]::GetEnvironmentVariable("Path", "Machine")
$MachineCommand = $MachinePath -split ";" | ForEach-Object {
    $Entry = [Environment]::ExpandEnvironmentVariables($_.Trim().Trim('"'))
    if (-not [string]::IsNullOrWhiteSpace($Entry)) {
        foreach ($Extension in $PathExtensions) {
            Join-Path $Entry "bitbygit$Extension"
        }
    }
} | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
if ($MachineCommand) {
    throw "Machine PATH already contains $MachineCommand. User PATH cannot override it in new shells. Remove or update that machine-level installation, then rerun; until then invoke $InstalledBinary explicitly."
}
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
$UserPathEntries = @($UserPath -split ";" | Where-Object {
    -not [string]::IsNullOrEmpty($_) -and $_ -ne $InstallDir
})
$NewUserPath = (@($InstallDir) + $UserPathEntries) -join ";"
$ProcessPathEntries = @($env:Path -split ";" | Where-Object {
    -not [string]::IsNullOrEmpty($_) -and $_ -ne $InstallDir
})
$PreviousProcessPath = $env:Path
try {
    $env:Path = (@($InstallDir) + $ProcessPathEntries) -join ";"
    $ResolvedCommand = Get-Command bitbygit -CommandType Application -ErrorAction Stop | Select-Object -First 1
    $ResolvedPath = (Resolve-Path -LiteralPath $ResolvedCommand.Source).Path
    $InstalledPath = (Resolve-Path -LiteralPath $InstalledBinary).Path
    if (-not [string]::Equals($ResolvedPath, $InstalledPath, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Expected $InstalledPath on PATH, got $ResolvedPath"
    }
    $PathOutput = & $ResolvedPath --version
    if ($LASTEXITCODE -ne 0 -or $PathOutput -ne "bitbygit $Version") {
        throw "Expected bitbygit $Version on PATH, got $PathOutput"
    }
    [Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")
} catch {
    $env:Path = $PreviousProcessPath
    throw
}
& $ResolvedPath --version
} finally {
    try {
        Remove-Item -LiteralPath $WorkDir -Recurse -Force
    } catch {
        [Console]::Error.WriteLine("Warning: failed to remove temporary directory ${WorkDir}: $($_.Exception.Message)")
    }
}
}
```

## Package managers

Package-manager distribution is deferred. `bitbygit` is not currently
published to crates.io, Homebrew, WinGet, Scoop, or Linux package repositories.
Use an archive or source tag after a release is published.

## Verify the installation

Every installation block above first runs the installed file directly, then
updates `PATH`, resolves an application while ignoring same-named aliases and
functions, and verifies that it is the installed file before reporting
`bitbygit <selected-version>`.
