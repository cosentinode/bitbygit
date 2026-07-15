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

Run these commands in Bash. They require `curl`, `tar`, `grep`, and GNU
coreutils (`sha256sum`, `mkdir`, and `install`) and stop before extraction or
installation if any download or checksum check fails:

```bash
bash -euo pipefail <<'BITBYGIT_INSTALL' &&
VERSION=0.1.0
TARGET=x86_64-unknown-linux-gnu
ARCHIVE="bitbygit-${VERSION}-${TARGET}.tar.gz"
PACKAGE="bitbygit-${VERSION}-${TARGET}"
BASE_URL="https://github.com/cosentinode/bitbygit/releases/download/v${VERSION}"

curl -fLO "${BASE_URL}/${ARCHIVE}"
curl -fLO "${BASE_URL}/SHA256SUMS"
grep -F "  ${ARCHIVE}" SHA256SUMS | sha256sum --check -
tar -xzf "${ARCHIVE}"

mkdir -p "${HOME}/.local/bin"
install -m 0755 "${PACKAGE}/bitbygit" "${HOME}/.local/bin/bitbygit"
output="$("${HOME}/.local/bin/bitbygit" --version)"
if [[ "${output}" != "bitbygit ${VERSION}" ]]; then
  printf 'Expected bitbygit %s, got %s\n' "${VERSION}" "${output}" >&2
  exit 1
fi
BITBYGIT_INSTALL
export PATH="${HOME}/.local/bin:${PATH}"
```

Add `export PATH="$HOME/.local/bin:$PATH"` to your shell startup file to keep
the command available in new shells.

## macOS archive

Run these commands in Bash. They select the Intel or Apple silicon artifact
automatically and require `curl`, `shasum`, and `tar`, which are included with
macOS. The shell stops before extraction or installation if any download or
checksum check fails:

```bash
bash -euo pipefail <<'BITBYGIT_INSTALL' &&
VERSION=0.1.0
case "$(uname -m)" in
  x86_64) TARGET=x86_64-apple-darwin ;;
  arm64) TARGET=aarch64-apple-darwin ;;
  *) echo "Unsupported macOS architecture: $(uname -m)" >&2; exit 1 ;;
esac
ARCHIVE="bitbygit-${VERSION}-${TARGET}.tar.gz"
PACKAGE="bitbygit-${VERSION}-${TARGET}"
BASE_URL="https://github.com/cosentinode/bitbygit/releases/download/v${VERSION}"

curl -fLO "${BASE_URL}/${ARCHIVE}"
curl -fLO "${BASE_URL}/SHA256SUMS"
grep -F "  ${ARCHIVE}" SHA256SUMS | shasum -a 256 --check -
tar -xzf "${ARCHIVE}"

mkdir -p "${HOME}/.local/bin"
install -m 0755 "${PACKAGE}/bitbygit" "${HOME}/.local/bin/bitbygit"
output="$("${HOME}/.local/bin/bitbygit" --version)"
if [[ "${output}" != "bitbygit ${VERSION}" ]]; then
  printf 'Expected bitbygit %s, got %s\n' "${VERSION}" "${output}" >&2
  exit 1
fi
BITBYGIT_INSTALL
export PATH="${HOME}/.local/bin:${PATH}"
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

Invoke-WebRequest -Uri "$BaseUrl/$Archive" -OutFile $Archive
Invoke-WebRequest -Uri "$BaseUrl/SHA256SUMS" -OutFile SHA256SUMS
$ExpectedLine = Get-Content SHA256SUMS | Where-Object { $_.EndsWith("  $Archive") }
if (-not $ExpectedLine) { throw "No checksum found for $Archive" }
$Expected = ($ExpectedLine -split '\s+')[0]
$Actual = (Get-FileHash -Algorithm SHA256 $Archive).Hash
if ($Actual -ne $Expected) { throw "Checksum verification failed for $Archive" }
Expand-Archive -Path $Archive -DestinationPath .

$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\bitbygit"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item "$Package\bitbygit.exe" $InstallDir
$InstalledBinary = Join-Path $InstallDir "bitbygit.exe"
$Output = & $InstalledBinary --version
if ($LASTEXITCODE -ne 0 -or $Output -ne "bitbygit $Version") {
    throw "Expected bitbygit $Version, got $Output"
}
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($UserPath -split ";") -notcontains $InstallDir) {
    [Environment]::SetEnvironmentVariable("Path", "$UserPath;$InstallDir", "User")
}
$env:Path = "$InstallDir;$env:Path"
}
```

New shells will use the updated user `PATH`.

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
bash -euo pipefail <<'BITBYGIT_INSTALL' &&
VERSION=0.1.0
git clone --branch "v${VERSION}" --depth 1 https://github.com/cosentinode/bitbygit.git
cd bitbygit
cargo build --locked --release -p bitbygit
built_output="$(target/release/bitbygit --version)"
if [[ "${built_output}" != "bitbygit ${VERSION}" ]]; then
  printf 'Expected bitbygit %s, got %s\n' "${VERSION}" "${built_output}" >&2
  exit 1
fi

mkdir -p "${HOME}/.local/bin"
install -m 0755 target/release/bitbygit "${HOME}/.local/bin/bitbygit"
installed_output="$("${HOME}/.local/bin/bitbygit" --version)"
if [[ "${installed_output}" != "bitbygit ${VERSION}" ]]; then
  printf 'Expected bitbygit %s, got %s\n' "${VERSION}" "${installed_output}" >&2
  exit 1
fi
BITBYGIT_INSTALL
export PATH="${HOME}/.local/bin:${PATH}"
```

On Windows, run in PowerShell:

```powershell
& {
$ErrorActionPreference = "Stop"

$Version = "0.1.0"
git clone --branch "v$Version" --depth 1 https://github.com/cosentinode/bitbygit.git
if ($LASTEXITCODE -ne 0) { throw "git clone failed" }
Set-Location bitbygit
cargo build --locked --release -p bitbygit
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
$BuiltOutput = & "target\release\bitbygit.exe" --version
if ($LASTEXITCODE -ne 0 -or $BuiltOutput -ne "bitbygit $Version") {
    throw "Expected bitbygit $Version, got $BuiltOutput"
}

$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\bitbygit"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item "target\release\bitbygit.exe" $InstallDir
$InstalledBinary = Join-Path $InstallDir "bitbygit.exe"
$InstalledOutput = & $InstalledBinary --version
if ($LASTEXITCODE -ne 0 -or $InstalledOutput -ne "bitbygit $Version") {
    throw "Expected bitbygit $Version, got $InstalledOutput"
}
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($UserPath -split ";") -notcontains $InstallDir) {
    [Environment]::SetEnvironmentVariable("Path", "$UserPath;$InstallDir", "User")
}
$env:Path = "$InstallDir;$env:Path"
}
```

## Package managers

Package-manager distribution is deferred. `bitbygit` is not currently
published to crates.io, Homebrew, WinGet, Scoop, or Linux package repositories.
Use an archive or source tag after a release is published.

## Verify the installation

Every installation block above finishes by running the installed file directly
with `--version` and requiring `bitbygit <selected-version>`. This avoids
mistaking an older `bitbygit` elsewhere on `PATH` for the binary just installed.
