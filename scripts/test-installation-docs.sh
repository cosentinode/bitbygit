#!/usr/bin/env bash

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
docs="${root}/docs/installation.md"
workflow="${root}/.github/workflows/release.yml"
tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT

fail() {
  echo "installation docs validation failed: $*" >&2
  exit 1
}

extract_block() {
  local heading="$1"
  local language="$2"
  HEADING="${heading}" LANGUAGE="${language}" perl -0777 -ne '
    $heading = quotemeta($ENV{HEADING});
    $language = quotemeta($ENV{LANGUAGE});
    if (/^## $heading\n.*?^```$language\n(.*?)^```/ms) {
      print $1;
    }
  ' "${docs}"
}

docs_text="$(<"${docs}")"
workflow_text="$(<"${workflow}")"
workspace_version="$(perl -ne '
  $workspace = 1 if /^\[workspace\.package\]$/;
  if ($workspace && /^version = "([^"]+)"$/) { print $1; exit }
' "${root}/Cargo.toml")"
[[ -n "${workspace_version}" ]] || fail "workspace version was not found"
[[ "${docs_text}" == *"VERSION=${workspace_version}"* ]] || fail "Bash examples do not use workspace version ${workspace_version}"
[[ "${docs_text}" == *"\$Version = \"${workspace_version}\""* ]] || fail "PowerShell examples do not use workspace version ${workspace_version}"

mapfile -t release_artifacts < <(perl -0777 -ne '
  while (/^\s+- runner:.*?^\s+target:\s+(\S+)\s*\n^\s+archive_extension:\s+(\S+)/msg) {
    print "bitbygit-<version>-$1.$2\n";
  }
' "${workflow}")
[[ "${#release_artifacts[@]}" -eq 4 ]] || fail "expected four release artifacts"
for artifact in "${release_artifacts[@]}"; do
  [[ "${docs_text}" == *"${artifact}"* ]] || fail "missing release artifact ${artifact}"
done
[[ "${workflow_text}" == *'package="bitbygit-${version}-${{ matrix.target }}"'* ]] || fail "release package directory contract changed"
[[ "${docs_text}" == *'PACKAGE="bitbygit-${VERSION}-${TARGET}"'* ]] || fail "Bash package directory does not match release layout"
[[ "${docs_text}" == *'$Package = "bitbygit-$Version-$Target"'* ]] || fail "PowerShell package directory does not match release layout"
[[ "${docs_text}" == *'"${PACKAGE}/bitbygit"'* ]] || fail "Unix binary path does not match release layout"
[[ "${docs_text}" == *'"$Package\bitbygit.exe"'* ]] || fail "Windows binary path does not match release layout"

perl -0777 -ne 'while (/^```bash\n(.*?)^```/msg) { print "$1\n" }' "${docs}" > "${tmp}/snippets.bash"
bash -n "${tmp}/snippets.bash"

perl -0777 -ne 'while (/^```powershell\n(.*?)^```/msg) { print "$1\n" }' "${docs}" > "${tmp}/snippets.ps1"
SNIPPETS_PS1="${tmp}/snippets.ps1" pwsh -NoProfile -NonInteractive -Command '
  $tokens = $null
  $errors = $null
  [System.Management.Automation.Language.Parser]::ParseFile($env:SNIPPETS_PS1, [ref]$tokens, [ref]$errors) | Out-Null
  if ($errors.Count -ne 0) { $errors | ForEach-Object { Write-Error $_ }; exit 1 }
'

mkdir -p "${tmp}/mock-bin"
cat > "${tmp}/mock-bin/curl" <<'MOCK'
#!/usr/bin/env bash
output="${*: -1}"
output="${output##*/}"
if [[ "${output}" == "SHA256SUMS" ]]; then
  printf '%064d  %s\n' 0 "${MOCK_ARCHIVE}" > "${output}"
else
  : > "${output}"
fi
MOCK
cat > "${tmp}/mock-bin/sha256sum" <<'MOCK'
#!/usr/bin/env bash
exit 1
MOCK
cat > "${tmp}/mock-bin/shasum" <<'MOCK'
#!/usr/bin/env bash
exit 1
MOCK
for command in tar install; do
  cat > "${tmp}/mock-bin/${command}" <<'MOCK'
#!/usr/bin/env bash
: > "${MOCK_SIDE_EFFECTS}"
MOCK
done
chmod +x "${tmp}/mock-bin/"*

check_unix_checksum_gate() {
  local heading="$1"
  local target="$2"
  local extension="$3"
  local case_dir="${tmp}/${target}"
  mkdir -p "${case_dir}/home"
  extract_block "${heading}" bash > "${case_dir}/snippet.bash"
  [[ -s "${case_dir}/snippet.bash" ]] || fail "missing ${heading} Bash block"

  if (
    cd "${case_dir}"
    PATH="${tmp}/mock-bin:${PATH}" \
      HOME="${case_dir}/home" \
      MOCK_ARCHIVE="bitbygit-${workspace_version}-${target}.${extension}" \
      MOCK_SIDE_EFFECTS="${case_dir}/side-effects" \
      bash "${case_dir}/snippet.bash"
  ); then
    fail "${heading} continued after checksum failure"
  fi
  [[ ! -e "${case_dir}/side-effects" ]] || fail "${heading} extracted or installed after checksum failure"
}

check_unix_checksum_gate "Linux x86-64 archive" x86_64-unknown-linux-gnu tar.gz
check_unix_checksum_gate "macOS archive" x86_64-apple-darwin tar.gz

extract_block "Windows x86-64 archive" powershell > "${tmp}/windows-install.ps1"
[[ -s "${tmp}/windows-install.ps1" ]] || fail "missing Windows PowerShell block"
mkdir -p "${tmp}/windows-case"
if MOCK_ARCHIVE="bitbygit-${workspace_version}-x86_64-pc-windows-msvc.zip" \
  MOCK_CHECKSUM_REACHED="${tmp}/windows-checksum-reached" \
  MOCK_SIDE_EFFECTS="${tmp}/windows-side-effects" \
  WINDOWS_CASE_DIR="${tmp}/windows-case" \
  WINDOWS_INSTALL_PS1="${tmp}/windows-install.ps1" \
  pwsh -NoProfile -NonInteractive -Command '
  Set-Location $env:WINDOWS_CASE_DIR
  function Invoke-WebRequest { param($Uri, $OutFile) Set-Content -LiteralPath $OutFile -Value "mock" }
  function Get-Content { "0000000000000000000000000000000000000000000000000000000000000000  $env:MOCK_ARCHIVE" }
  function Get-FileHash {
    Set-Content -LiteralPath $env:MOCK_CHECKSUM_REACHED -Value "checked"
    [pscustomobject]@{ Hash = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff" }
  }
  function Expand-Archive { Set-Content -LiteralPath $env:MOCK_SIDE_EFFECTS -Value "expanded"; throw "continued" }
  & $env:WINDOWS_INSTALL_PS1
' >/dev/null 2>&1; then
  fail "Windows archive block continued after checksum failure"
fi
[[ -e "${tmp}/windows-checksum-reached" ]] || fail "Windows archive block did not reach checksum verification"
[[ ! -e "${tmp}/windows-side-effects" ]] || fail "Windows archive extracted after checksum failure"

[[ "${docs_text}" == *'git clone --branch "v${VERSION}" --depth 1'* ]] || fail "Bash source build is not pinned to the selected tag"
[[ "${docs_text}" == *'git clone --branch "v$Version" --depth 1'* ]] || fail "PowerShell source build is not pinned to the selected tag"
[[ "${docs_text}" == *'"${HOME}/.local/bin/bitbygit" --version'* ]] || fail "Unix installation does not validate the installed path"
[[ "${docs_text}" == *'& $InstalledBinary --version'* ]] || fail "Windows installation does not validate the installed path"

check_links() {
  local markdown="$1"
  local base
  base="$(dirname "${markdown}")"
  while IFS= read -r destination; do
    case "${destination}" in
      https://*) curl -fsSL --retry 3 --output /dev/null "${destination}" || fail "unreachable link ${destination}" ;;
      http://*) fail "insecure link ${destination}" ;;
      *)
        local fragment=""
        local target="${destination%%#*}"
        if [[ "${destination}" == *#* ]]; then
          fragment="${destination#*#}"
        fi
        if [[ -z "${target}" ]]; then
          target="${markdown}"
        else
          target="${base}/${target}"
        fi
        [[ -e "${target}" ]] || fail "missing local link ${destination} in ${markdown#"${root}/"}"
        if [[ -n "${fragment}" ]]; then
          ANCHOR="${fragment}" perl -ne '
            next unless /^#+\s+(.+)/;
            $heading = lc $1;
            $heading =~ s/[`*_]//g;
            $heading =~ s/[^a-z0-9 -]//g;
            $heading =~ s/\s+/-/g;
            if ($heading eq $ENV{ANCHOR}) { $found = 1; last }
            END { exit($found ? 0 : 1) }
          ' "${target}" || fail "missing anchor #${fragment} in ${target#"${root}/"}"
        fi
        ;;
    esac
  done < <(perl -ne 'while (/\[[^]]+\]\(([^ )]+)(?:\s+"[^"]*")?\)/g) { print "$1\n" }' "${markdown}")
}

check_links "${root}/README.md"
check_links "${docs}"

echo "installation docs validation passed"
