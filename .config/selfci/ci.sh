#!/usr/bin/env bash
set -eou pipefail

function job_cargo() {
  selfci step start "Cargo.lock up-to-date"
  if ! cargo update --workspace --locked -q; then
    selfci step fail
  fi

  selfci step start "build"
  if ! nix build -L .#ci.workspace; then
    selfci step fail
  fi

  selfci step start "clippy"
  if ! nix build -L .#ci.clippy; then
    selfci step fail
  fi

  selfci step start "nextest"
  if ! nix build -L .#ci.tests; then
    selfci step fail
  fi
}

function job_coverage() {
  selfci step start "coverage tests"
  if ! nix build -L .#ci.testsCcov; then
    selfci step fail
  fi

  selfci step start "cargo-crap report"
  if ! nix build -L .#ci.crapReport; then
    >&2 echo "cargo-crap: failed - CRAP report generation failed"
    selfci step fail
  fi
}

case "$SELFCI_JOB_NAME" in
  main)
    selfci job start "cargo"
    selfci job start "coverage"
    ;;
  cargo)
    job_cargo
    ;;
  coverage)
    job_coverage
    ;;
  *)
    echo "Unknown job: $SELFCI_JOB_NAME"
    exit 1
    ;;
esac
