#!/usr/bin/env bash

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workflow="${root}/.github/workflows/release.yml"
workflow_text="$(<"${workflow}")"
publish_script="$(perl -0777 -ne '
  if (/      - name: Verify and publish release assets.*?        run: \|\n(.*)\z/s) {
    $script = $1;
    $script =~ s/^          //mg;
    print $script;
  }
' "${workflow}")"

[[ -n "${publish_script}" ]]
[[ "${workflow_text}" == *"if: github.event_name == 'push' && github.ref_type == 'tag' && startsWith(github.ref_name, 'v')"* ]]
[[ "${workflow_text}" == *$'permissions:\n  contents: read'* ]]
[[ "${workflow_text}" == *$'    permissions:\n      contents: write'* ]]
[[ "${workflow_text}" == *$'concurrency:\n  group: release-${{ github.ref }}\n  cancel-in-progress: false'* ]]
if perl -ne 'exit 1 if /^\s*uses:\s+\S+@(?![0-9a-f]{40}(?:\s|$))/' "${workflow}"; then
  :
else
  echo "workflow contains a mutable action reference" >&2
  exit 1
fi

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT

run_case() {
  local scenario="$1"
  local case_dir="${tmp}/${scenario}"
  mkdir -p "${case_dir}/dist"

  for archive in \
    bitbygit-0.1.0-x86_64-unknown-linux-gnu.tar.gz \
    bitbygit-0.1.0-x86_64-apple-darwin.tar.gz \
    bitbygit-0.1.0-aarch64-apple-darwin.tar.gz \
    bitbygit-0.1.0-x86_64-pc-windows-msvc.zip; do
    printf 'mock archive\n' > "${case_dir}/dist/${archive}"
  done

  (
    cd "${case_dir}"
    export GH_REPO="cosentinode/bitbygit"
    export GITHUB_REF_NAME="v0.1.0"
    export MOCK_API_COUNT="${case_dir}/api-count"
    export MOCK_IMMUTABILITY_COUNT="${case_dir}/immutability-count"
    export MOCK_LOG="${case_dir}/gh.log"
    export MOCK_SCENARIO="${scenario}"
    export MOCK_STATE="${case_dir}/release-state"
    export RELEASE_SHA="1111111111111111111111111111111111111111"
    export VERSION="0.1.0"

    if [[ "${scenario}" == "retry" ]]; then
      printf 'draft' > "${MOCK_STATE}"
    elif [[ "${scenario}" == "published" ]]; then
      printf 'public' > "${MOCK_STATE}"
    fi

    gh() {
      printf '%q ' "$@" >> "${MOCK_LOG}"
      printf '\n' >> "${MOCK_LOG}"

      if [[ "$1" == "api" ]]; then
        if [[ "$2" == *"/immutable-releases" ]]; then
          local count=0
          [[ ! -f "${MOCK_IMMUTABILITY_COUNT}" ]] || count="$(<"${MOCK_IMMUTABILITY_COUNT}")"
          count=$((count + 1))
          printf '%s' "${count}" > "${MOCK_IMMUTABILITY_COUNT}"
          if [[ "${MOCK_SCENARIO}" == "disabled" ]] || \
            [[ "${MOCK_SCENARIO}" == "drift" && "${count}" -ge 2 ]]; then
            printf 'false\n'
          else
            printf 'true\n'
          fi
        elif [[ "$2" == *"/git/ref/tags/"* ]]; then
          local count=0
          [[ ! -f "${MOCK_API_COUNT}" ]] || count="$(<"${MOCK_API_COUNT}")"
          count=$((count + 1))
          printf '%s' "${count}" > "${MOCK_API_COUNT}"
          if [[ "${MOCK_SCENARIO}" == "moved" && "${count}" -ge 2 ]]; then
            printf 'commit\t2222222222222222222222222222222222222222\n'
          else
            printf 'tag\tannotated-tag-object\n'
          fi
        else
          printf 'commit\t%s\n' "${RELEASE_SHA}"
        fi
      elif [[ "$1 $2" == "release view" && "$*" == *"isDraft"* ]]; then
        [[ -f "${MOCK_STATE}" ]] || return 1
        [[ "$(<"${MOCK_STATE}")" == "draft" ]] && printf 'true\n' || printf 'false\n'
      elif [[ "$1 $2" == "release view" ]]; then
        if [[ "${MOCK_SCENARIO}" == "mutable" ]]; then
          printf 'false\n'
        else
          printf 'true\n'
        fi
      elif [[ "$1 $2" == "release create" ]]; then
        printf 'draft' > "${MOCK_STATE}"
        [[ "${MOCK_SCENARIO}" != "partial" ]]
      elif [[ "$1 $2" == "release edit" ]]; then
        printf 'public' > "${MOCK_STATE}"
      elif [[ "$1 $2" == "release delete" ]]; then
        rm -f "${MOCK_STATE}"
      fi
    }
    export -f gh

    bash -n <<< "${publish_script}"
    bash -c "${publish_script}"
  )
}

run_case success
success_log="$(<"${tmp}/success/gh.log")"
[[ "${success_log}" == *"release create v0.1.0 --repo cosentinode/bitbygit --target 1111111111111111111111111111111111111111 --verify-tag --draft"* ]]
[[ "${success_log}" == *"release edit v0.1.0 --repo cosentinode/bitbygit --verify-tag --draft=false"* ]]
[[ "${success_log}" == *"release view v0.1.0 --repo cosentinode/bitbygit --json isImmutable"* ]]
[[ "$(<"${tmp}/success/api-count")" == "3" ]]
[[ "$(<"${tmp}/success/immutability-count")" == "2" ]]

run_case retry
retry_log="$(<"${tmp}/retry/gh.log")"
[[ "${retry_log}" == *"release delete v0.1.0 --repo cosentinode/bitbygit --yes"* ]]
[[ "${retry_log}" == *"release create v0.1.0"* ]]

if run_case published; then
  echo "existing published release unexpectedly replaced" >&2
  exit 1
fi
published_log="$(<"${tmp}/published/gh.log")"
[[ "${published_log}" != *"release delete"* ]]
[[ "${published_log}" != *"release create"* ]]

if run_case partial; then
  echo "partial upload unexpectedly succeeded" >&2
  exit 1
fi
partial_log="$(<"${tmp}/partial/gh.log")"
[[ "${partial_log}" == *"release create v0.1.0"* ]]
[[ "${partial_log}" == *"release delete v0.1.0 --repo cosentinode/bitbygit --yes"* ]]
[[ ! -f "${tmp}/partial/release-state" ]]

if run_case moved; then
  echo "moved tag unexpectedly published" >&2
  exit 1
fi
moved_log="$(<"${tmp}/moved/gh.log")"
[[ "${moved_log}" == *"release create"* ]]
[[ "${moved_log}" != *"release edit"* ]]
[[ "${moved_log}" == *"release delete v0.1.0 --repo cosentinode/bitbygit --yes"* ]]

if run_case mutable; then
  echo "mutable release unexpectedly passed verification" >&2
  exit 1
fi
mutable_log="$(<"${tmp}/mutable/gh.log")"
[[ "${mutable_log}" == *"release edit v0.1.0"* ]]
[[ "${mutable_log}" == *"release delete v0.1.0 --repo cosentinode/bitbygit --yes"* ]]

if run_case disabled; then
  echo "release published with immutable releases disabled" >&2
  exit 1
fi
disabled_log="$(<"${tmp}/disabled/gh.log")"
[[ "${disabled_log}" != *"release create"* ]]

if run_case drift; then
  echo "release published after immutable releases were disabled" >&2
  exit 1
fi
drift_log="$(<"${tmp}/drift/gh.log")"
[[ "${drift_log}" == *"release create"* ]]
[[ "${drift_log}" != *"release edit"* ]]
[[ "${drift_log}" == *"release delete v0.1.0 --repo cosentinode/bitbygit --yes"* ]]

echo "release workflow validation passed"
