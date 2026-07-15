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

release_artifacts=()
while IFS= read -r artifact; do
  release_artifacts+=("${artifact}")
done < <(perl -0777 -ne '
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

mkdir -p "${tmp}/assets" "${tmp}/mock-bin"
for target in x86_64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin; do
  package="bitbygit-${workspace_version}-${target}"
  mkdir -p "${tmp}/package/${package}"
  printf '#!/usr/bin/env sh\nprintf '\''bitbygit %s\\n'\''\n' "${workspace_version}" > "${tmp}/package/${package}/bitbygit"
  chmod +x "${tmp}/package/${package}/bitbygit"
  tar -C "${tmp}/package" -czf "${tmp}/assets/${package}.tar.gz" "${package}"
  rm -rf "${tmp}/package/${package}"
done
(
  cd "${tmp}/assets"
  for archive in bitbygit-*.tar.gz; do
    if command -v sha256sum >/dev/null; then
      digest="$(sha256sum "${archive}")"
    else
      digest="$(shasum -a 256 "${archive}")"
    fi
    printf '%s  %s\n' "${digest%% *}" "${archive}"
  done > SHA256SUMS
)

cat > "${tmp}/mock-bin/curl" <<'MOCK'
#!/usr/bin/env bash
for source in "$@"; do :; done
cp "${MOCK_DOWNLOAD_DIR}/${source##*/}" .
MOCK
cat > "${tmp}/mock-bin/uname" <<'MOCK'
#!/usr/bin/env bash
if [[ "${1:-}" == "-m" ]]; then
  printf '%s\n' "${MOCK_UNAME_MACHINE}"
else
  /usr/bin/uname "$@"
fi
MOCK
chmod +x "${tmp}/mock-bin/"*

check_unix_success() {
  local heading="$1"
  local target="$2"
  local machine="$3"
  local case_dir="${tmp}/success-${target}"
  mkdir -p "${case_dir}/home"
  extract_block "${heading}" bash > "${case_dir}/snippet.bash"
  [[ -s "${case_dir}/snippet.bash" ]] || fail "missing ${heading} Bash block"

  (
    cd "${case_dir}"
    set +e
    set +u
    set +o pipefail
    export HOME="${case_dir}/home"
    export MOCK_DOWNLOAD_DIR="${tmp}/assets"
    export MOCK_UNAME_MACHINE="${machine}"
    export PATH="${tmp}/mock-bin:${PATH}"

    source "${case_dir}/snippet.bash" || fail "${heading} failed a valid ${target} installation"
    [[ "$-" != *e* && "$-" != *u* ]] || fail "${heading} changed caller shell options"
    if shopt -qo pipefail; then
      fail "${heading} enabled pipefail in the caller shell"
    fi
    [[ "${PATH%%:*}" == "${HOME}/.local/bin" ]] || fail "${heading} did not update the caller PATH"
    [[ -x "${HOME}/.local/bin/bitbygit" ]] || fail "${heading} did not install the binary"
    [[ "$("${HOME}/.local/bin/bitbygit" --version)" == "bitbygit ${workspace_version}" ]] || fail "${heading} installed the wrong version"
  )
}

host_os="${INSTALLATION_DOCS_TEST_OS:-$(uname -s)}"
case "${host_os}" in
  Linux)
    check_unix_success "Linux x86-64 archive" x86_64-unknown-linux-gnu x86_64
    failure_heading="Linux x86-64 archive"
    failure_target=x86_64-unknown-linux-gnu
    failure_machine=x86_64
    ;;
  Darwin)
    check_unix_success "macOS archive" x86_64-apple-darwin x86_64
    check_unix_success "macOS archive" aarch64-apple-darwin arm64
    failure_heading="macOS archive"
    failure_target=x86_64-apple-darwin
    failure_machine=x86_64
    ;;
  *) fail "unsupported validation host ${host_os}" ;;
esac

mkdir -p "${tmp}/failure-assets" "${tmp}/failure-bin"
cp "${tmp}/assets/bitbygit-${workspace_version}-${failure_target}.tar.gz" "${tmp}/failure-assets/"
printf '%064d  bitbygit-%s-%s.tar.gz\n' 0 "${workspace_version}" "${failure_target}" > "${tmp}/failure-assets/SHA256SUMS"
cp "${tmp}/mock-bin/curl" "${tmp}/failure-bin/curl"
cp "${tmp}/mock-bin/uname" "${tmp}/failure-bin/uname"
for command in tar install; do
  cat > "${tmp}/failure-bin/${command}" <<'MOCK'
#!/usr/bin/env bash
: > "${MOCK_SIDE_EFFECTS}"
MOCK
done
chmod +x "${tmp}/failure-bin/"*

failure_dir="${tmp}/failure-${failure_target}"
mkdir -p "${failure_dir}/home"
extract_block "${failure_heading}" bash > "${failure_dir}/snippet.bash"
(
  cd "${failure_dir}"
  set +e
  set +u
  set +o pipefail
  original_path="${tmp}/failure-bin:${PATH}"
  export HOME="${failure_dir}/home"
  export MOCK_DOWNLOAD_DIR="${tmp}/failure-assets"
  export MOCK_SIDE_EFFECTS="${failure_dir}/side-effects"
  export MOCK_UNAME_MACHINE="${failure_machine}"
  export PATH="${original_path}"
  if source "${failure_dir}/snippet.bash"; then
    fail "${failure_heading} continued after checksum failure"
  fi
  [[ "$-" != *e* && "$-" != *u* ]] || fail "failed ${failure_heading} changed caller shell options"
  if shopt -qo pipefail; then
    fail "failed ${failure_heading} enabled pipefail in the caller shell"
  fi
  [[ "${PATH}" == "${original_path}" ]] || fail "failed ${failure_heading} changed the caller PATH"
)
[[ ! -e "${failure_dir}/side-effects" ]] || fail "${failure_heading} extracted or installed after checksum failure"

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

echo "installation docs validation passed on ${host_os}"
