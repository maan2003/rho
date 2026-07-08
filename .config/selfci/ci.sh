#!/usr/bin/env bash
set -eou pipefail

function job_cargo() {
  selfci step start "Cargo.lock up-to-date"
  if ! cargo update --workspace --locked -q; then
    selfci step fail
  fi

  selfci step start "build"
  if ! cargo build --locked --workspace --all-targets; then
    selfci step fail
  fi

  selfci step start "clippy"
  if ! cargo clippy --locked --workspace --all-targets -- --deny warnings --allow deprecated; then
    selfci step fail
  fi

  selfci step start "test"
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
