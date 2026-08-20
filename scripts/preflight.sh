#!/usr/bin/env bash
# CI-aligned preflight gate for VexPortal.
#
# There is no upstream .github/ CI workflow to mirror, so this script is built
# directly from CLAUDE.md's Phase 3 project-specific build validation steps.
# Every command runs through `nix develop -c ...` so GTK4/libadwaita/glib are
# on PKG_CONFIG_PATH — never bare `cargo`, never `nix shell` (see CLAUDE.md
# FORBIDDEN COMMANDS).
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

echo "==> cargo fmt --check"
nix develop -c cargo fmt --all -- --check

echo "==> cargo check --workspace"
nix develop -c cargo check --workspace

echo "==> cargo clippy --workspace --all-targets"
nix develop -c cargo clippy --workspace --all-targets

echo "==> cargo test --workspace"
nix develop -c cargo test --workspace

echo "==> preflight passed"
