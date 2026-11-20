#!/usr/bin/env bash
# Open a shell driving disposable rho GUI and host fixtures. See README.md.
set -euo pipefail
out=$(realpath -m "${1:?usage: capture.sh OUTPUT_DIRECTORY [BIN_DIRECTORY]}")
root=$(git rev-parse --show-toplevel)
bins=$(realpath "${2:-$root/target/debug}")
for name in rho-gui rho-agent-host rho-agent-worker rho-fake-model; do
    test -x "$bins/$name" || { printf 'missing binary: %s/%s\n' "$bins" "$name" >&2; exit 1; }
done
rig=$(mktemp -d)
session="form-${rig##*.}"
model_pid=
host_pid=
cleanup() {
    local size pid
    for size in laptop phone; do
        form_way "$size" stop >/dev/null 2>&1 || true
    done
    for pid in "$host_pid" "$model_pid"; do
        if [[ -n "$pid" ]]; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    rm -rf -- "$rig"
}
trap cleanup EXIT
form_way() {
    local size=$1
    shift
    rho wayland --session "$session-$size" "$@"
}
form_gui() {
    local size=$1 width height scale client
    case "$size" in
        laptop) width=1300 height=850 scale=1 ;;
        phone) width=780 height=1688 scale=2 ;;
        *) printf 'size must be laptop or phone\n' >&2; return 2 ;;
    esac
    # An optional existing fixture identity lets the phone display the same
    # UI as the laptop without pairing. Stop the original window first.
    client=${2:-$size}
    mkdir -p "$rig/$client"/{state,config,cache,data}
    form_way "$size" start --width "$width" --height "$height" --scale "$scale" -- \
        dbus-run-session -- env \
        -u RHO_GUI_FONTS -u RHO_GUI_FONT_FAMILY -u RHO_GUI_FONT_SCALE \
        -u RHO_GUI_FONT_WEIGHT -u RHO_GUI_FONT_BOLD_WEIGHT \
        XDG_STATE_HOME="$rig/$client/state" XDG_CONFIG_HOME="$rig/$client/config" \
        XDG_CACHE_HOME="$rig/$client/cache" XDG_DATA_HOME="$rig/$client/data" \
        "$bins/rho-gui" --attach "lab=local:$rig/runtime"
}
form_shot() {
    form_way "$1" screenshot --output "$out/$1-$2.png"
}
mkdir -p "$out" "$rig"/{host-state/rho/auth.d,host-config,host-cache,host-data,claude,repo}
GIT_CONFIG_GLOBAL=/dev/null git -C "$rig/repo" init -q -b main
GIT_CONFIG_GLOBAL=/dev/null git -C "$rig/repo" \
    -c user.name=Fixture -c user.email=fixture@example.invalid commit --allow-empty -qm fixture
python3 - <<'PY' >"$rig/host-state/rho/auth.d/default.json"
import json

print(json.dumps({
    "access_token": "rho-form-fixture",
    "expires_at_ms": 2**64 - 1,
    "account_id": "form-fixture",
    "client_secret": [0] * 32,
}))
PY
"$bins/rho-fake-model" --scenario clarifying-question --no-faults >"$rig/model.json" 2>"$rig/model.log" &
model_pid=$!
for _ in {1..100}; do
    [[ -s "$rig/model.json" ]] && break
    sleep .05
done
openai=$(jq -er .openai_base_url "$rig/model.json")
anthropic=$(jq -er .anthropic_base_url "$rig/model.json")
env PATH="$bins:$PATH" \
    XDG_STATE_HOME="$rig/host-state" XDG_CONFIG_HOME="$rig/host-config" \
    XDG_CACHE_HOME="$rig/host-cache" XDG_DATA_HOME="$rig/host-data" \
    "$bins/rho-agent-host" --runtime-dir "$rig/runtime" --claude-config-dir "$rig/claude" \
    --openai-base-url "$openai" --anthropic-base-url "$anthropic" >"$rig/host.log" 2>&1 &
host_pid=$!
for _ in {1..200}; do
    if grep -q 'rho-agent-host: ready' "$rig/host.log"; then break; fi
    sleep .05
done
grep 'rho-agent-host: ready' "$rig/host.log" || { cat "$rig/host.log" >&2; exit 1; }
export rig out bins session
export -f form_way form_gui form_shot
printf 'Fixture: %s\nOutput:  %s\nRun the capture commands from product/design/README.md. Exit to stop both windows and the fixture host.\n' "$rig" "$out"
bash --noprofile --norc
