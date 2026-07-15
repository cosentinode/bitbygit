#!/usr/bin/env bash

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
docs="${root}/docs/installation.md"
workflow="${root}/.github/workflows/release.yml"
ci_workflow="${root}/.github/workflows/ci.yml"
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
ci_workflow_text="$(<"${ci_workflow}")"
workspace_version="$(perl -ne '
  $workspace = 1 if /^\[workspace\.package\]$/;
  if ($workspace && /^version = "([^"]+)"$/) { print $1; exit }
' "${root}/Cargo.toml")"
[[ -n "${workspace_version}" ]] || fail "workspace version was not found"
selected_version=2.3.4
[[ "${selected_version}" != "${workspace_version}" && "${selected_version}" != "0.1.0" ]] || fail "selected validator version must differ from the documented example"
[[ "${docs_text}" == *"VERSION=${workspace_version}"* ]] || fail "Bash examples do not use workspace version ${workspace_version}"
[[ "${docs_text}" == *"\$Version = \"${workspace_version}\""* ]] || fail "PowerShell examples do not use workspace version ${workspace_version}"

extract_selected_block() {
  local block
  block="$(extract_block "$1" "$2")"
  [[ "${block}" == *"VERSION=${workspace_version}"* ]] || fail "missing selected version in $1 $2 block"
  printf '%s' "${block/VERSION=${workspace_version}/VERSION=${selected_version}}"
}

extract_selected_path_block() {
  extract_selected_block "$1" bash | perl -0777 -ne '
    if (/^BITBYGIT_INSTALL\n(.*)\z/ms) { print $1 }
  '
}

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
[[ "${ci_workflow_text}" == *"toolchain: 1.85.0"* ]] || fail "CI does not pin the documented minimum Rust version"
for target in x86_64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin x86_64-pc-windows-msvc; do
  [[ "${ci_workflow_text}" == *"target: ${target}"* ]] || fail "Rust 1.85 CI does not build ${target}"
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
  package="bitbygit-${selected_version}-${target}"
  mkdir -p "${tmp}/package/${package}"
  printf '#!/usr/bin/env sh\nprintf '\''bitbygit %s\\n'\''\n' "${selected_version}" > "${tmp}/package/${package}/bitbygit"
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
cat > "${tmp}/mock-bin/bitbygit" <<'MOCK'
#!/usr/bin/env sh
printf 'bitbygit 9.9.9\n'
MOCK
chmod +x "${tmp}/mock-bin/"*

check_unix_success() {
  local heading="$1"
  local target="$2"
  local machine="$3"
  local case_dir="${tmp}/success-${target}"
  local archive="bitbygit-${selected_version}-${target}.tar.gz"
  local package="bitbygit-${selected_version}-${target}"
  mkdir -p "${case_dir}/home" "${case_dir}/temp" "${case_dir}/protected-package"
  printf 'protected\n' > "${case_dir}/protected-file"
  printf 'protected\n' > "${case_dir}/protected-package/marker"
  ln -s protected-file "${case_dir}/${archive}"
  ln -s protected-file "${case_dir}/SHA256SUMS"
  ln -s protected-package "${case_dir}/${package}"
  extract_selected_block "${heading}" bash > "${case_dir}/snippet.bash"
  extract_selected_path_block "${heading}" > "${case_dir}/path-snippet.bash"
  [[ -s "${case_dir}/snippet.bash" ]] || fail "missing ${heading} Bash block"

  (
    cd "${case_dir}"
    set +e
    set +u
    set +o pipefail
    export HOME="${case_dir}/home"
    export TMPDIR="${case_dir}/temp"
    export MOCK_DOWNLOAD_DIR="${tmp}/assets"
    export MOCK_UNAME_MACHINE="${machine}"
    base_path="${PATH}"
    expected_path="${HOME}/.local/bin::${tmp}/mock-bin::${base_path}:"
    export PATH=":${tmp}/mock-bin::${HOME}/.local/bin:${base_path}:${HOME}/.local/bin:"
    starting_dir="${PWD}"

    bitbygit() {
      : > "${case_dir}/function-shadow-used"
      printf 'bitbygit %s\n' "${selected_version}"
    }
    source "${case_dir}/snippet.bash" || fail "${heading} failed a valid ${target} installation"
    [[ ! -e "${case_dir}/function-shadow-used" ]] || fail "${heading} invoked a shadowing function"
    unset -f bitbygit
    shadow_bitbygit() {
      : > "${case_dir}/alias-shadow-used"
      printf 'bitbygit %s\n' "${selected_version}"
    }
    shopt -s expand_aliases
    alias bitbygit=shadow_bitbygit
    source "${case_dir}/snippet.bash" || fail "${heading} failed when retried"
    [[ ! -e "${case_dir}/alias-shadow-used" ]] || fail "${heading} invoked a shadowing alias"
    unalias bitbygit
    unset -f shadow_bitbygit
    [[ "${PWD}" == "${starting_dir}" ]] || fail "${heading} changed the caller working directory"
    [[ "$-" != *e* && "$-" != *u* ]] || fail "${heading} changed caller shell options"
    if shopt -qo pipefail; then
      fail "${heading} enabled pipefail in the caller shell"
    fi
    [[ "${PATH}" == "${expected_path}" ]] || fail "${heading} did not preserve empty PATH entries while deduplicating the install directory"
    path_entry_count=0
    IFS=: read -r -a path_entries <<< "${PATH}"
    for entry in "${path_entries[@]}"; do
      [[ "${entry}" == "${HOME}/.local/bin" ]] && path_entry_count=$((path_entry_count + 1))
    done
    [[ "${path_entry_count}" -eq 1 ]] || fail "${heading} duplicated the install directory on PATH"
    [[ -x "${HOME}/.local/bin/bitbygit" ]] || fail "${heading} did not install the binary"
    [[ "$("${HOME}/.local/bin/bitbygit" --version)" == "bitbygit ${selected_version}" ]] || fail "${heading} installed the wrong version"
    resolved_binary="$(type -P bitbygit)"
    [[ "${resolved_binary}" -ef "${HOME}/.local/bin/bitbygit" ]] || fail "${heading} did not resolve the installed application through PATH"
    [[ "$(command bitbygit --version)" == "bitbygit ${selected_version}" ]] || fail "${heading} resolved the wrong version through PATH"
    default_path="$(command -p getconf PATH)"
    unset PATH
    source "${case_dir}/path-snippet.bash" || fail "${heading} failed with an originally unset PATH"
    [[ "${PATH}" == "${HOME}/.local/bin:${default_path}" ]] || fail "${heading} did not retain default command lookup for an originally unset PATH"
    [[ -n "$(type -P ls)" ]] || fail "${heading} lost standard command lookup for an originally unset PATH"
    if compgen -G "${TMPDIR}/*" >/dev/null; then
      fail "${heading} left temporary installation files behind"
    fi
  )
  [[ -L "${case_dir}/${archive}" && "$(readlink "${case_dir}/${archive}")" == protected-file ]] || fail "${heading} replaced the pre-existing archive symlink"
  [[ -L "${case_dir}/SHA256SUMS" && "$(readlink "${case_dir}/SHA256SUMS")" == protected-file ]] || fail "${heading} replaced the pre-existing checksum symlink"
  [[ -L "${case_dir}/${package}" && "$(readlink "${case_dir}/${package}")" == protected-package ]] || fail "${heading} replaced the pre-existing package symlink"
  [[ "$(<"${case_dir}/protected-file")" == protected ]] || fail "${heading} modified a symlink target"
  [[ "$(<"${case_dir}/protected-package/marker")" == protected ]] || fail "${heading} extracted through a package symlink"
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
cp "${tmp}/assets/bitbygit-${selected_version}-${failure_target}.tar.gz" "${tmp}/failure-assets/"
printf '%064d  bitbygit-%s-%s.tar.gz\n' 0 "${selected_version}" "${failure_target}" > "${tmp}/failure-assets/SHA256SUMS"
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
mkdir -p "${failure_dir}/home" "${failure_dir}/temp"
extract_selected_block "${failure_heading}" bash > "${failure_dir}/snippet.bash"
(
  cd "${failure_dir}"
  set +e
  set +u
  set +o pipefail
  original_path="${tmp}/failure-bin:${PATH}"
  export HOME="${failure_dir}/home"
  export TMPDIR="${failure_dir}/temp"
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
  if compgen -G "${TMPDIR}/*" >/dev/null; then
    fail "failed ${failure_heading} left temporary installation files behind"
  fi
)
[[ ! -e "${failure_dir}/side-effects" ]] || fail "${failure_heading} extracted or installed after checksum failure"

collision_dir="${tmp}/checksum-collision-${failure_target}"
collision_assets="${tmp}/checksum-collision-assets"
mkdir -p "${collision_dir}/home" "${collision_dir}/temp" "${collision_dir}/mock-bin" "${collision_assets}"
collision_archive="bitbygit-${selected_version}-${failure_target}.tar.gz"
cp "${tmp}/assets/${collision_archive}" "${collision_assets}/${collision_archive}"
printf 'malicious sidecar\n' > "${collision_assets}/${collision_archive}.sig"
if command -v sha256sum >/dev/null; then
  collision_digest="$(sha256sum "${collision_assets}/${collision_archive}.sig")"
else
  collision_digest="$(shasum -a 256 "${collision_assets}/${collision_archive}.sig")"
fi
printf '%s  %s.sig\n' "${collision_digest%% *}" "${collision_archive}" > "${collision_assets}/SHA256SUMS"
cp "${tmp}/failure-bin/uname" "${tmp}/failure-bin/tar" "${tmp}/failure-bin/install" "${collision_dir}/mock-bin/"
cat > "${collision_dir}/mock-bin/curl" <<'MOCK'
#!/usr/bin/env bash
for source in "$@"; do :; done
name="${source##*/}"
cp "${MOCK_DOWNLOAD_DIR}/${name}" .
if [[ -f "${MOCK_DOWNLOAD_DIR}/${name}.sig" ]]; then
  cp "${MOCK_DOWNLOAD_DIR}/${name}.sig" .
fi
MOCK
chmod +x "${collision_dir}/mock-bin/"*
extract_selected_block "${failure_heading}" bash > "${collision_dir}/snippet.bash"
(
  cd "${collision_dir}"
  original_path="${collision_dir}/mock-bin:${PATH}"
  export HOME="${collision_dir}/home"
  export TMPDIR="${collision_dir}/temp"
  export MOCK_DOWNLOAD_DIR="${collision_assets}"
  export MOCK_SIDE_EFFECTS="${collision_dir}/side-effects"
  export MOCK_UNAME_MACHINE="${failure_machine}"
  export PATH="${original_path}"
  if source "${collision_dir}/snippet.bash"; then
    fail "${failure_heading} accepted a checksum for a similarly named file"
  fi
  [[ "${PATH}" == "${original_path}" ]] || fail "checksum collision changed the caller PATH"
  if compgen -G "${TMPDIR}/*" >/dev/null; then
    fail "checksum collision left temporary installation files behind"
  fi
)
[[ ! -e "${collision_dir}/side-effects" ]] || fail "${failure_heading} extracted or installed without an exact checksum entry"

cleanup_failure_dir="${tmp}/cleanup-failure-${failure_target}"
mkdir -p "${cleanup_failure_dir}/home" "${cleanup_failure_dir}/temp" "${cleanup_failure_dir}/mock-bin"
cp "${tmp}/failure-bin/"* "${cleanup_failure_dir}/mock-bin/"
cat > "${cleanup_failure_dir}/mock-bin/rm" <<'MOCK'
#!/usr/bin/env sh
exit 73
MOCK
chmod +x "${cleanup_failure_dir}/mock-bin/rm"
extract_selected_block "${failure_heading}" bash > "${cleanup_failure_dir}/snippet.bash"
(
  cd "${cleanup_failure_dir}"
  export HOME="${cleanup_failure_dir}/home"
  export TMPDIR="${cleanup_failure_dir}/temp"
  export MOCK_DOWNLOAD_DIR="${tmp}/failure-assets"
  export MOCK_SIDE_EFFECTS="${cleanup_failure_dir}/side-effects"
  export MOCK_UNAME_MACHINE="${failure_machine}"
  export PATH="${cleanup_failure_dir}/mock-bin:${PATH}"
  if source "${cleanup_failure_dir}/snippet.bash" > "${cleanup_failure_dir}/output" 2>&1; then
    fail "${failure_heading} accepted an invalid checksum when cleanup failed"
  else
    cleanup_status=$?
  fi
  [[ "${cleanup_status}" -eq 1 ]] || fail "${failure_heading} cleanup failure masked status 1 with ${cleanup_status}"
  cleanup_output="$(<"${cleanup_failure_dir}/output")"
  [[ "${cleanup_output}" == *"Warning: failed to remove temporary directory"* ]] || fail "${failure_heading} did not report cleanup failure"
)
[[ ! -e "${cleanup_failure_dir}/side-effects" ]] || fail "${failure_heading} continued after checksum and cleanup failures"

[[ "${docs_text}" == *'git -C "${SOURCE_DIR}" fetch --depth 1 https://github.com/cosentinode/bitbygit.git "${TAG}:${TAG}"'* ]] || fail "Bash source build does not fetch the exact selected tag"
[[ "${docs_text}" == *'git -C "${SOURCE_DIR}" checkout --detach "${tag_commit}"'* ]] || fail "Bash source build does not detach at the selected tag"
[[ "${docs_text}" == *'git -C $SourceDir fetch --depth 1 https://github.com/cosentinode/bitbygit.git "${Tag}:${Tag}"'* ]] || fail "PowerShell source build does not fetch the exact selected tag"
[[ "${docs_text}" == *'git -C $SourceDir checkout --detach $TagCommit'* ]] || fail "PowerShell source build does not detach at the selected tag"
[[ "${docs_text}" == *'"${HOME}/.local/bin/bitbygit" --version'* ]] || fail "Unix installation does not validate the installed path"
[[ "${docs_text}" == *'& $InstalledBinary --version'* ]] || fail "Windows installation does not validate the installed path"
[[ "$(grep -c 'resolved_binary="$(type -P bitbygit || true)"' "${docs}")" -eq 3 ]] || fail "Unix installations do not bypass aliases and functions during PATH resolution"
[[ "$(grep -c -- '-ef "${install_dir}/bitbygit"' "${docs}")" -eq 3 ]] || fail "Unix installations do not verify resolved file identity"
[[ "$(grep -c '^  command bitbygit --version$' "${docs}")" -eq 3 ]] || fail "Unix installations do not finish with PATH-resolved version output"
[[ "$(grep -c '^& \$ResolvedPath --version$' "${docs}")" -eq 2 ]] || fail "Windows installations do not finish with PATH-resolved version output"

source_remote="${tmp}/source-remote.git"
source_seed="${tmp}/source-seed"
git init --bare --quiet "${source_remote}"
git init --quiet "${source_seed}"
git -C "${source_seed}" config user.email validator@example.invalid
git -C "${source_seed}" config user.name "Installation validator"
mkdir -p "${source_seed}/target/release"
printf '#!/usr/bin/env sh\nprintf '\''bitbygit %s\\n'\''\n' "${selected_version}" > "${source_seed}/target/release/bitbygit"
chmod +x "${source_seed}/target/release/bitbygit"
git -C "${source_seed}" add -f target/release/bitbygit
git -C "${source_seed}" commit --quiet -m "tagged source"
git -C "${source_seed}" tag -a "v${selected_version}" -m "release ${selected_version}"
printf '#!/usr/bin/env sh\nprintf '\''bitbygit 9.9.9\\n'\''\n' > "${source_seed}/target/release/bitbygit"
git -C "${source_seed}" commit --quiet -am "same-named branch"
git -C "${source_seed}" branch "v${selected_version}"
git -C "${source_seed}" push --quiet "${source_remote}" \
  "refs/heads/v${selected_version}:refs/heads/v${selected_version}" \
  "refs/tags/v${selected_version}:refs/tags/v${selected_version}"

source_dir="${tmp}/source-install"
mkdir -p "${source_dir}/home" "${source_dir}/mock-bin" "${source_dir}/temp" "${source_dir}/protected-source"
printf 'protected\n' > "${source_dir}/protected-source/marker"
ln -s protected-source "${source_dir}/bitbygit"
cat > "${source_dir}/mock-bin/cargo" <<'MOCK'
#!/usr/bin/env sh
if [ "${MOCK_CARGO_FAILURE:-0}" = 1 ]; then
  exit 1
fi
exit 0
MOCK
cp "${tmp}/mock-bin/bitbygit" "${source_dir}/mock-bin/bitbygit"
chmod +x "${source_dir}/mock-bin/"*
source_snippet="$(extract_selected_block "Build from source" bash)"
source_snippet="${source_snippet/https:\/\/github.com\/cosentinode\/bitbygit.git/${source_remote}}"
printf '%s' "${source_snippet}" > "${source_dir}/snippet.bash"
extract_selected_path_block "Build from source" > "${source_dir}/path-snippet.bash"
(
  cd "${source_dir}"
  export HOME="${source_dir}/home"
  export TMPDIR="${source_dir}/temp"
  base_path="${PATH}"
  original_path=":${source_dir}/mock-bin::${HOME}/.local/bin:${base_path}:${HOME}/.local/bin:"
  expected_path="${HOME}/.local/bin::${source_dir}/mock-bin::${base_path}:"
  export PATH="${original_path}"
  starting_dir="${PWD}"
  export MOCK_CARGO_FAILURE=1
  if source "${source_dir}/snippet.bash"; then
    fail "Bash source block continued after a failed build"
  fi
  [[ "${PWD}" == "${starting_dir}" ]] || fail "failed Bash source block changed the caller working directory"
  [[ "${PATH}" == "${original_path}" ]] || fail "failed Bash source block changed the caller PATH"
  if compgen -G "${TMPDIR}/*" >/dev/null; then
    fail "failed Bash source block left temporary build files behind"
  fi

  export MOCK_CARGO_FAILURE=0
  bitbygit() {
    : > "${source_dir}/function-shadow-used"
    printf 'bitbygit %s\n' "${selected_version}"
  }
  source "${source_dir}/snippet.bash" || fail "Bash source block could not retry a failed build"
  [[ ! -e "${source_dir}/function-shadow-used" ]] || fail "Bash source block invoked a shadowing function"
  unset -f bitbygit
  shadow_bitbygit() {
    : > "${source_dir}/alias-shadow-used"
    printf 'bitbygit %s\n' "${selected_version}"
  }
  shopt -s expand_aliases
  alias bitbygit=shadow_bitbygit
  source "${source_dir}/snippet.bash" || fail "Bash source block failed when retried"
  [[ ! -e "${source_dir}/alias-shadow-used" ]] || fail "Bash source block invoked a shadowing alias"
  unalias bitbygit
  unset -f shadow_bitbygit
  [[ "${PWD}" == "${starting_dir}" ]] || fail "Bash source block changed the caller working directory"
  [[ "${PATH}" == "${expected_path}" ]] || fail "Bash source block did not preserve empty PATH entries while deduplicating the install directory"
  path_entry_count=0
  IFS=: read -r -a path_entries <<< "${PATH}"
  for entry in "${path_entries[@]}"; do
    [[ "${entry}" == "${HOME}/.local/bin" ]] && path_entry_count=$((path_entry_count + 1))
  done
  [[ "${path_entry_count}" -eq 1 ]] || fail "Bash source block duplicated the install directory on PATH"
  resolved_binary="$(type -P bitbygit)"
  [[ "${resolved_binary}" -ef "${HOME}/.local/bin/bitbygit" ]] || fail "Bash source block did not resolve the installed application through PATH"
  [[ "$(command bitbygit --version)" == "bitbygit ${selected_version}" ]] || fail "Bash source block resolved the wrong version through PATH"
  default_path="$(command -p getconf PATH)"
  unset PATH
  source "${source_dir}/path-snippet.bash" || fail "Bash source block failed with an originally unset PATH"
  [[ "${PATH}" == "${HOME}/.local/bin:${default_path}" ]] || fail "Bash source block did not retain default command lookup for an originally unset PATH"
  [[ -n "$(type -P git)" ]] || fail "Bash source block lost standard command lookup for an originally unset PATH"
  if compgen -G "${TMPDIR}/*" >/dev/null; then
    fail "Bash source block left temporary build files behind"
  fi
)
[[ -L "${source_dir}/bitbygit" && "$(readlink "${source_dir}/bitbygit")" == protected-source ]] || fail "Bash source block replaced a pre-existing source symlink"
[[ "$(<"${source_dir}/protected-source/marker")" == protected ]] || fail "Bash source block wrote through a pre-existing source symlink"

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
