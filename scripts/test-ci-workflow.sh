#!/usr/bin/env bash

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workflow="${root}/.github/workflows/ci.yml"
test_source="$(<"${root}/crates/bitbygit-git/src/lib.rs")"
recovery_job="$(perl -0777 -ne '
  print $1 if /\n  recovery-platforms:\n(.*?)(?=\n  [a-z][a-z0-9-]*:\n|\z)/s
' "${workflow}")"

expected='cargo test --locked -p bitbygit-git recovery_platform_'
if [[ -z "${recovery_job}" || "${recovery_job}" != *"${expected}"* ]]; then
  printf 'CI workflow does not run end-to-end recovery on supported platforms\n' >&2
  exit 1
fi
if [[ "${recovery_job}" != *'macos-latest'* || "${recovery_job}" != *'windows-latest'* ]]; then
  printf 'CI workflow does not cover macOS and Windows recovery\n' >&2
  exit 1
fi
for test_name in \
  recovery_platform_command_output_is_bounded \
  recovery_platform_end_to_end_deadline_does_not_interrupt_merge_abort \
  recovery_platform_end_to_end_exact_merge_and_rebase \
  recovery_platform_execution_cleans_descendants_after_parent_exit \
  recovery_platform_execution_cleans_windows_job_descendants_after_parent_exit; do
  if [[ "${test_source}" != *"fn ${test_name}"* ]]; then
    printf 'CI recovery selector is missing test %s\n' "${test_name}" >&2
    exit 1
  fi
done

printf 'CI workflow validation passed\n'
