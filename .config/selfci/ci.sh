#!/usr/bin/env bash
set -eou pipefail

function job_cargo() {
  selfci step start "Cargo.lock up-to-date"
  if ! cargo update --workspace --locked -q; then
    selfci step fail
  fi

  selfci step start "build"
  if ! cargo build --locked --workspace --all-targets || \
    ! cargo build --locked --package jj-cli --bin jj; then
    selfci step fail
  fi

  selfci step start "clippy"
  if ! cargo clippy --locked --workspace --all-targets -- --deny warnings --allow deprecated || \
    ! cargo clippy --locked --package jj-cli --bin jj -- --deny warnings --allow deprecated; then
    selfci step fail
  fi

  selfci step start "test"
  # Workspace tests exercise the bundled jj fork's managed-workspace CLI.
  # The build step above produced it; prefer that binary over an installed jj.
  export PATH="${CARGO_TARGET_DIR:-$PWD/target}/debug:$PATH"
  if ! cargo test --locked --workspace; then
    selfci step fail
  fi
}

case "$SELFCI_JOB_NAME" in
  main)
    selfci job start "cargo"
    ;;
  cargo)
    job_cargo
    ;;
  *)
    echo "Unknown job: $SELFCI_JOB_NAME"
    exit 1
    ;;
esac
