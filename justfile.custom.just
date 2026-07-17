# Project-local just extensions for flakebox.

# Check formatting without changing files. The generated final-check recipe
# depends on this project-local hook.
lint:
  treefmt --ci

# Build an optimized rho-gui and save a sampled CPU profile for later analysis.
profile-gui OUTPUT="rho-gui-profile.folded" *ARGS="":
  cargo build --profile profiling --package rho-gui
  target/profiling/rho-gui --cpu-profile "{{OUTPUT}}" {{ARGS}}

# Build an optimized daemon and save all-thread CPU stacks on shutdown.
profile-daemon OUTPUT="rho-daemon-profile.folded" *ARGS="":
  cargo build --profile profiling --package rho-cli
  target/profiling/rho daemon --cpu-profile "{{OUTPUT}}" {{ARGS}}
