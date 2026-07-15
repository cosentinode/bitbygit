# Installation

`bitbygit` can be installed from a GitHub Release archive or built from source.
No tagged release has been published yet. Use the archive instructions once the
version you want appears on the
[Releases page](https://github.com/cosentinode/bitbygit/releases).

## Runtime prerequisites

- `git` must be installed and available on `PATH`.
- [GitHub CLI (`gh`)](https://cli.github.com/) is optional. It is required only
  for GitHub-specific operations such as opening a pull request; run
  `gh auth login` before using those operations.
- Rust is not required when using a release archive. Building from source
  requires Rust 1.85 or newer and Cargo.

## Supported release targets

| Platform | Target | Archive |
| --- | --- | --- |
| Linux x86-64 | `x86_64-unknown-linux-gnu` | `bitbygit-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| macOS Intel | `x86_64-apple-darwin` | `bitbygit-<version>-x86_64-apple-darwin.tar.gz` |
| macOS Apple silicon | `aarch64-apple-darwin` | `bitbygit-<version>-aarch64-apple-darwin.tar.gz` |
| Windows x86-64 | `x86_64-pc-windows-msvc` | `bitbygit-<version>-x86_64-pc-windows-msvc.zip` |

The Linux artifact uses GNU libc and is checked not to require a GLIBC symbol
newer than 2.35. Other architectures and operating systems must currently use
a source build.

Each tagged release also includes `SHA256SUMS`. Verify the downloaded archive
before extracting or running it. The examples below use `0.1.0`; set `VERSION`
or `$Version` to an available release version without the leading `v`.

## Linux x86-64 archive

The commands require `curl`, `sha256sum`, and `tar`:

```sh
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
export PATH="${HOME}/.local/bin:${PATH}"
```

Add `export PATH="$HOME/.local/bin:$PATH"` to your shell startup file to keep
the command available in new shells.

## macOS archive

This selects the Intel or Apple silicon artifact automatically. The commands
require `curl`, `shasum`, and `tar`, which are included with macOS:

```sh
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
export PATH="${HOME}/.local/bin:${PATH}"
```

Add `export PATH="$HOME/.local/bin:$PATH"` to `~/.zprofile` (or the startup file
for your shell) to keep the command available in new shells.

## Windows x86-64 archive

Run these commands in PowerShell:

```powershell
$Version = "0.1.0"
$Target = "x86_64-pc-windows-msvc"
$Archive = "bitbygit-$Version-$Target.zip"
$Package = "bitbygit-$Version-$Target"
$BaseUrl = "https://github.com/cosentinode/bitbygit/releases/download/v$Version"

Invoke-WebRequest -Uri "$BaseUrl/$Archive" -OutFile $Archive
Invoke-WebRequest -Uri "$BaseUrl/SHA256SUMS" -OutFile SHA256SUMS
$Expected = ((Get-Content SHA256SUMS | Where-Object { $_.EndsWith("  $Archive") }) -split '\s+')[0]
$Actual = (Get-FileHash -Algorithm SHA256 $Archive).Hash
if ($Actual -ne $Expected) { throw "Checksum verification failed for $Archive" }
Expand-Archive -Path $Archive -DestinationPath .

$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\bitbygit"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item "$Package\bitbygit.exe" $InstallDir
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($UserPath -split ";") -notcontains $InstallDir) {
    [Environment]::SetEnvironmentVariable("Path", "$UserPath;$InstallDir", "User")
}
$env:Path = "$InstallDir;$env:Path"
```

New shells will use the updated user `PATH`.

## Build from source

Install [Rust](https://www.rust-lang.org/tools/install) 1.85 or newer, Cargo,
and `git`, then build the locked workspace package:

```sh
git clone https://github.com/cosentinode/bitbygit.git
cd bitbygit
cargo build --locked --release -p bitbygit
```

On Linux or macOS, install the resulting binary in the same user directory used
above:

```sh
mkdir -p "${HOME}/.local/bin"
install -m 0755 target/release/bitbygit "${HOME}/.local/bin/bitbygit"
export PATH="${HOME}/.local/bin:${PATH}"
```

On Windows, run this in PowerShell from the repository:

```powershell
$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\bitbygit"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item "target\release\bitbygit.exe" $InstallDir
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($UserPath -split ";") -notcontains $InstallDir) {
    [Environment]::SetEnvironmentVariable("Path", "$UserPath;$InstallDir", "User")
}
$env:Path = "$InstallDir;$env:Path"
```

## Package managers

Package-manager distribution is deferred. `bitbygit` is not currently
published to crates.io, Homebrew, WinGet, Scoop, or Linux package repositories.
Use a release archive when available or build from source.

## Verify the installation

The final check for every installation method is:

```sh
bitbygit --version
```
