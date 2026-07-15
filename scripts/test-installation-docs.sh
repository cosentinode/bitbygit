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

[[ "${docs_text}" == *'git -C bitbygit fetch --depth 1 https://github.com/cosentinode/bitbygit.git "${TAG}:${TAG}"'* ]] || fail "Bash source build does not fetch the exact selected tag"
[[ "${docs_text}" == *'git -C bitbygit checkout --detach "${tag_commit}"'* ]] || fail "Bash source build does not detach at the selected tag"
[[ "${docs_text}" == *'git -C $SourceDir fetch --depth 1 https://github.com/cosentinode/bitbygit.git "${Tag}:${Tag}"'* ]] || fail "PowerShell source build does not fetch the exact selected tag"
[[ "${docs_text}" == *'git -C $SourceDir checkout --detach $TagCommit'* ]] || fail "PowerShell source build does not detach at the selected tag"
[[ "${docs_text}" == *'"${HOME}/.local/bin/bitbygit" --version'* ]] || fail "Unix installation does not validate the installed path"
[[ "${docs_text}" == *'& $InstalledBinary --version'* ]] || fail "Windows installation does not validate the installed path"

source_remote="${tmp}/source-remote.git"
source_seed="${tmp}/source-seed"
git init --bare --quiet "${source_remote}"
git init --quiet "${source_seed}"
git -C "${source_seed}" config user.email validator@example.invalid
git -C "${source_seed}" config user.name "Installation validator"
mkdir -p "${source_seed}/target/release"
printf '#!/usr/bin/env sh\nprintf '\''bitbygit %s\\n'\''\n' "${workspace_version}" > "${source_seed}/target/release/bitbygit"
chmod +x "${source_seed}/target/release/bitbygit"
git -C "${source_seed}" add -f target/release/bitbygit
git -C "${source_seed}" commit --quiet -m "tagged source"
git -C "${source_seed}" tag -a "v${workspace_version}" -m "release ${workspace_version}"
printf '#!/usr/bin/env sh\nprintf '\''bitbygit 9.9.9\\n'\''\n' > "${source_seed}/target/release/bitbygit"
git -C "${source_seed}" commit --quiet -am "same-named branch"
git -C "${source_seed}" branch "v${workspace_version}"
git -C "${source_seed}" push --quiet "${source_remote}" \
  "refs/heads/v${workspace_version}:refs/heads/v${workspace_version}" \
  "refs/tags/v${workspace_version}:refs/tags/v${workspace_version}"

source_dir="${tmp}/source-install"
mkdir -p "${source_dir}/home" "${source_dir}/mock-bin"
cat > "${source_dir}/mock-bin/cargo" <<'MOCK'
#!/usr/bin/env sh
exit 0
MOCK
chmod +x "${source_dir}/mock-bin/cargo"
source_snippet="$(extract_block "Build from source" bash)"
source_snippet="${source_snippet/https:\/\/github.com\/cosentinode\/bitbygit.git/${source_remote}}"
printf '%s' "${source_snippet}" > "${source_dir}/snippet.bash"
(
  cd "${source_dir}"
  export HOME="${source_dir}/home"
  export PATH="${source_dir}/mock-bin:${PATH}"
  source "${source_dir}/snippet.bash" || fail "Bash source block failed with colliding branch and tag names"
  tag_commit="$(git --git-dir="${source_remote}" rev-parse "refs/tags/v${workspace_version}^{commit}")"
  branch_commit="$(git --git-dir="${source_remote}" rev-parse "refs/heads/v${workspace_version}")"
  head_commit="$(git -C bitbygit rev-parse HEAD)"
  [[ "${head_commit}" == "${tag_commit}" && "${head_commit}" != "${branch_commit}" ]] || fail "Bash source block did not check out the exact tag object"
)

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
