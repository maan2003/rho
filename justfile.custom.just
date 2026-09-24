# Project-local just extensions for flakebox.

# Check formatting without changing files. The generated final-check recipe
# depends on this project-local hook.
lint:
  treefmt --ci

# Build an optimized rho-gui and save a sampled CPU profile for later analysis.
profile-gui OUTPUT="rho-gui-profile.bin" *ARGS="":
  RUSTFLAGS="${RUSTFLAGS:-} --cfg tokio_unstable -Cforce-frame-pointers=yes" cargo build --profile profiling --package rho-gui
  target/profiling/rho-gui --cpu-profile "{{OUTPUT}}" {{ARGS}}

# Build an optimized daemon and save a Dial9 trace on shutdown.
profile-daemon OUTPUT="rho-agent-host-profile.bin" *ARGS="":
  RUSTFLAGS="${RUSTFLAGS:-} --cfg tokio_unstable -Cforce-frame-pointers=yes" cargo build --profile profiling --package rho-cli
  target/profiling/rho daemon --cpu-profile "{{OUTPUT}}" {{ARGS}}

# Fail if anything the agent host builds reaches the GUI: gpui, a GUI crate,
# or the `client` half of a protocol crate. Features are resolved per
# package, as a host-only build resolves them; the dev shell's cargo
# otherwise unifies them across the whole workspace.
check-split:
  #!/usr/bin/env bash
  set -euo pipefail
  gui='^(gpui|rho-gui|rho-window|rho-slack|rho-transcript|rho-agents-view|rho-journal|rho-browser|rho-browser-wayland) |^rho-[a-z-]+ feature "client"'
  status=0
  for manifest in agent-host/*/Cargo.toml agent-host/rho-git/*/Cargo.toml; do
    package=$(sed -n 's/^name = "\(.*\)"$/\1/p' "$manifest" | head -n1)
    leaks=$(cargo tree -p "$package" -e normal,features --prefix none \
      --config 'resolver.feature-unification="package"' -Zfeature-unification \
      | grep -E "$gui" | sort -u || true)
    if [ -n "$leaks" ]; then
      echo "$package reaches the GUI:"
      echo "$leaks"
      status=1
    fi
  done
  exit $status
