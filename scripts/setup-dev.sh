#!/usr/bin/env sh
set -eu

bash scripts/test-ci-workflow.sh
bash scripts/test-release-workflow.sh
bash scripts/test-installation-docs.sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked --workspace
