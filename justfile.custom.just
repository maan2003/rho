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
profile-daemon OUTPUT="rho-daemon-profile.bin" *ARGS="":
  RUSTFLAGS="${RUSTFLAGS:-} --cfg tokio_unstable -Cforce-frame-pointers=yes" cargo build --profile profiling --package rho-cli
  target/profiling/rho daemon --cpu-profile "{{OUTPUT}}" {{ARGS}}

# Build the reproducible webui bundle and push it as a fresh root commit to
# the `gh-pages` bookmark of this repo's origin (GitHub Pages). Deploys from
# a separate local jj repo so single-use deploy commits never accumulate in
# this checkout's repository. Override the deploy checkout location with
# RHO_WEBUI_DEPLOY_DIR.
deploy-webui:
  #!/usr/bin/env bash
  set -euo pipefail
  bundle="$(nix build .#webui --print-out-paths)"
  remote="$(jj git remote list | awk '$1 == "origin" { print $2; exit }')"
  if [ -z "$remote" ]; then
    echo "error: no 'origin' git remote configured" >&2
    exit 1
  fi
  # The deploy repo pushes over plain SSH, outside rho's octo transport.
  if [[ "$remote" == octo://* ]]; then
    remote="${remote#octo://}"
    remote="git@${remote%%/*}:${remote#*/}"
  fi
  deploy="${RHO_WEBUI_DEPLOY_DIR:-$HOME/.local/share/rho/webui-deploy}"
  if [ ! -e "$deploy/.jj" ]; then
    mkdir -p "$deploy"
    (cd "$deploy" && jj git init . && jj git remote add origin "$remote")
  fi
  cd "$deploy"
  jj git remote set-url origin "$remote"
  # The wasm bundle exceeds jj's default new-file snapshot cap.
  jj config set --repo snapshot.max-new-file-size 67108864
  jj new 'root()' -m "deploy $(basename "$bundle")"
  cp -r --no-preserve=mode,ownership "$bundle"/. .
  # Pages runs Jekyll on branch deploys unless told not to.
  touch .nojekyll
  jj bookmark set gh-pages --allow-backwards -r @
  jj git push --bookmark gh-pages
  # Only the latest deploy commit is reachable; let jj drop the rest.
  jj util gc --expire now
