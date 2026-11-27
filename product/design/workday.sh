#!/usr/bin/env bash
# The heavy-workday rig, sourced by capture-matrix.sh and qa-session.sh: a
# seeded device and three hosts in the middle of a messy day. See README.md.
#
#   lab      live host + rho-fake-model (clarifying-question, reports an
#            allowance); projects tide, rho, infra
#   ci       live host + its own fake model (real-tool-rounds), which goes
#            away during the day; project nightly
#   gpu-box  away: the device holds its log, no host runs; projects tide
#            (so tide@lab and tide@gpu-box) and train
#
# Everything lives under one temporary directory; nothing reaches a network
# beyond 127.0.0.1.

# workday_isolate SCRIPT ARG...: runs SCRIPT again in a network namespace
# of its own, as the same user, with only loopback. The hosts and the window
# use iroh, which would otherwise reach n0's relays; here nothing can leave
# the machine. WORKDAY_NETWORK=host skips it.
workday_isolate() {
    if [[ -n "${WORKDAY_NETNS:-}" || "${WORKDAY_NETWORK:-isolated}" == host ]]; then
        return 0
    fi
    # shellcheck disable=SC2016 # the inner shell expands these
    exec unshare --user --map-root-user --net -- bash -c '
        ip link set lo up || exit 1
        uid=$1 gid=$2
        shift 2
        exec unshare --user --map-user="$uid" --map-group="$gid" -- env WORKDAY_NETNS=1 "$@"
    ' workday "$(id -u)" "$(id -g)" bash "$@"
}

# workday_build: the binaries and the fixture, in one cargo run.
workday_build() {
    local root
    root=$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)
    (
        cd "$root" || exit
        cargo build --quiet -p rho-agent-host --example heavy_workday \
            -p rho-gui -p rho-inference -p rho-fake-model -p rho-cli --bins
    )
}

# workday_init BIN_DIRECTORY: checks the binaries and makes the rig.
workday_init() {
    local name
    workday_root=$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)
    bins=$(realpath "${1:-$workday_root/target/debug}")
    for name in rho-gui rho-agent-host rho-agent-worker rho-fake-model rho examples/heavy_workday; do
        test -x "$bins/$name" || {
            printf 'missing binary: %s/%s\nBuild it as product/design/README.md says.\n' "$bins" "$name" >&2
            return 1
        }
    done
    command -v rho-agent-desktop >/dev/null || {
        printf 'rho-agent-desktop is not on PATH; rho wayland needs it\n' >&2
        return 1
    }
    rig=$(mktemp -d "${TMPDIR:-/tmp}/rho-workday.XXXXXX")
    session="workday-${rig##*.}"
    # The rig's own runtime directory: the compositor's sockets and every
    # iroh client's IPC record stay in the rig, away from the user's
    # hosts and windows.
    mkdir -m 700 "$rig/run"
    XDG_RUNTIME_DIR="$rig/run"
    export rig bins session XDG_RUNTIME_DIR
}

workday_log() {
    printf '[workday %6.2fs] %s\n' "$(( $(date +%s%N) - workday_t0 ))e-9" "$*" >&2
}

# workday_seed: the repositories, host configuration and every store.
workday_seed() {
    local host project
    workday_t0=$(date +%s%N)
    for project in tide rho infra nightly; do
        mkdir -p "$rig/repos/$project"
        GIT_CONFIG_GLOBAL=/dev/null git -C "$rig/repos/$project" init -q -b main
        printf '# %s\n' "$project" >"$rig/repos/$project/README.md"
        GIT_CONFIG_GLOBAL=/dev/null git -C "$rig/repos/$project" add README.md
        GIT_CONFIG_GLOBAL=/dev/null git -C "$rig/repos/$project" -c user.name=Fixture \
            -c user.email=fixture@example.invalid commit -qm "Start $project"
    done
    "$bins/examples/heavy_workday" seed "$rig"
    for host in lab ci; do
        mkdir -p "$rig/$host"/{state/rho/auth.d,config/rho,cache,data,claude,runtime}
        # A credential the fake model accepts; it never leaves this machine.
        jq -n --arg host "$host" '{access_token: "rho-workday-\($host)",
            expires_at_ms: 18446744073709551615, account_id: "workday-\($host)",
            client_secret: [range(32) | 0]}' >"$rig/$host/state/rho/auth.d/default.json"
        # Claude needs mount overlays the fixture cannot have: the same
        # flow runs on the native runtime.
        cat >"$rig/$host/config/rho/init.py" <<'PY'
from rho.default import new_agent as default_new_agent

def new_agent(*, runtime, **kwargs):
    if runtime["kind"] == "claude":
        runtime = {"kind": "native", "model": "sol", "effort": "medium"}
    return default_new_agent(runtime=runtime, **kwargs)
PY
    done
    workday_log "seeded $rig"
}

# workday_model HOST ARG...: a fake model for HOST, ready on return.
workday_model() {
    local host=$1
    shift
    "$bins/rho-fake-model" --no-faults "$@" >"$rig/$host/model.json" 2>"$rig/$host/model.log" &
    echo $! >"$rig/$host/model.pid"
}

workday_wait_model() {
    local host=$1
    for _ in {1..200}; do
        [[ -s "$rig/$host/model.json" ]] && return 0
        sleep .025
    done
    cat "$rig/$host/model.log" >&2
    return 1
}

# workday_host HOST: starts HOST's agent host against its model.
workday_host() {
    local host=$1 openai anthropic
    workday_wait_model "$host"
    # The log goes on across restarts; `ready` counts from here.
    stat -c %s "$rig/$host/host.log" >"$rig/$host/log.offset" 2>/dev/null || echo 0 >"$rig/$host/log.offset"
    openai=$(jq -er .openai_base_url "$rig/$host/model.json")
    anthropic=$(jq -er .anthropic_base_url "$rig/$host/model.json")
    env PATH="$bins:$PATH" \
        XDG_STATE_HOME="$rig/$host/state" XDG_CONFIG_HOME="$rig/$host/config" \
        XDG_CACHE_HOME="$rig/$host/cache" XDG_DATA_HOME="$rig/$host/data" \
        "$bins/rho-agent-host" --runtime-dir "$rig/$host/runtime" \
        --claude-config-dir "$rig/$host/claude" \
        --openai-base-url "$openai" --anthropic-base-url "$anthropic" \
        >>"$rig/$host/host.log" 2>&1 &
    echo $! >"$rig/$host/host.pid"
}

workday_wait_host() {
    local host=$1
    for _ in {1..400}; do
        tail -c +"$(($(cat "$rig/$host/log.offset") + 1))" "$rig/$host/host.log" 2>/dev/null |
            grep -q 'rho-agent-host: ready' && return 0
        kill -0 "$(cat "$rig/$host/host.pid")" 2>/dev/null || break
        sleep .025
    done
    printf 'host %s did not start:\n' "$host" >&2
    tail -n 40 "$rig/$host/host.log" >&2
    return 1
}

# workday_kill_host HOST: the host drops, as a crash or a lost machine does.
workday_kill_host() {
    local pid
    pid=$(cat "$rig/$1/host.pid" 2>/dev/null) || return 0
    kill -9 "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
}

# workday_boot: models, hosts and ci's day, everything but the window.
workday_boot() {
    workday_model lab --scenario clarifying-question --report-allowance --seed 51
    workday_model ci --scenario real-tool-rounds --rounds 3 --seed 899421
    workday_host lab
    workday_host ci
    workday_wait_host lab
    workday_wait_host ci
    workday_log "lab and ci ready"
    "$bins/examples/heavy_workday" ci-day "$rig" "$(cat "$rig/ci/model.pid")" >"$rig/ci-day.json"
    workday_log "ci's day: $(jq -c . "$rig/ci-day.json")"
}

# workday_way SESSION ARG...: rho wayland, with recording from this build.
# The session lives in the rig, so any agent can drive it.
workday_way() {
    local name=$1
    shift
    "$bins/rho" wayland --state-dir "$rig/wayland" --session "$name" "$@"
}

# workday_gui SESSION: the device's window at laptop size, attached to all
# three hosts; returns once it is focused and has drawn.
workday_gui() {
    local name=$1
    mkdir -p "$rig/device"/{state,config,cache,data}
    # shellcheck disable=SC2016 # sh expands these
    workday_way "$name" start --width 1300 --height 850 --scale 1 -- \
        dbus-run-session -- env \
        -u RHO_GUI_FONTS -u RHO_GUI_FONT_FAMILY -u RHO_GUI_FONT_SCALE \
        -u RHO_GUI_FONT_WEIGHT -u RHO_GUI_FONT_BOLD_WEIGHT \
        RUST_LOG="${RUST_LOG:-info}" \
        XDG_STATE_HOME="$rig/device/state" XDG_CONFIG_HOME="$rig/device/config" \
        XDG_CACHE_HOME="$rig/device/cache" XDG_DATA_HOME="$rig/device/data" \
        sh -c 'exec "$@" >>"$0" 2>&1' "$rig/device/gui.log" \
        "$bins/rho-gui" --attach "lab=local:$rig/lab/runtime" \
        --attach "ci=local:$rig/ci/runtime" \
        --attach "gpu-box=local:$rig/gpu-box/runtime" >/dev/null
    for _ in {1..100}; do
        if workday_way "$name" tree 2>/dev/null | jq -e \
            '[.. | objects | select(.app_id? == "rho-gui" and .is_focused? == true)] | length == 1' \
            >/dev/null; then
            workday_log "window focused"
            return 0
        fi
        sleep .1
    done
    printf 'rho-gui did not open a focused window\n' >&2
    workday_way "$name" status >&2 || true
    return 1
}

# workday_helpers: commands in $rig/bin that drive the rig from anywhere,
# entering its network namespace when it has one. QA agents use these.
workday_helpers() {
    local enter=
    mkdir -p "$rig/bin"
    if [[ -n "${WORKDAY_NETNS:-}" ]]; then
        enter="nsenter -t $$ -U -n --preserve-credentials --"
    fi
    cat >"$rig/bin/inside" <<EOF
#!/usr/bin/env bash
# inside COMMAND...: runs COMMAND where the rig's hosts and window run.
export XDG_RUNTIME_DIR="$rig/run"
exec $enter "\$@"
EOF
    cat >"$rig/bin/way" <<EOF
#!/usr/bin/env bash
# way ARG...: rho wayland on the rig's window, with record.
exec "$rig/bin/inside" "$bins/rho" wayland --state-dir "$rig/wayland" --session "$session" "\$@"
EOF
    cat >"$rig/bin/host" <<EOF
#!/usr/bin/env bash
# host kill|start lab|ci: drops a host as a crash does, or starts it again.
set -euo pipefail
case "\$1" in
    kill) kill -9 "\$(cat "$rig/\$2/host.pid")" ;;
    start) exec "$rig/bin/inside" env rig="$rig" bins="$bins" bash -c 'source "\$0"; workday_host "\$1"; workday_wait_host "\$1"' \\
        "${BASH_SOURCE[0]}" "\$2" ;;
    *) printf 'usage: host kill|start HOST\\n' >&2; exit 2 ;;
esac
EOF
    cat >"$rig/bin/say" <<EOF
#!/usr/bin/env bash
# say THREAD TEXT [flash]: an agent posts in a seeded thread, such as
# lab/flaky; fixture.json names them all.
set -euo pipefail
host=\$(jq -er --arg t "\$1" '.threads[\$t].host' "$rig/fixture.json")
id=\$(jq -er --arg t "\$1" '.threads[\$t].id' "$rig/fixture.json")
shift
exec "$rig/bin/inside" "$bins/examples/heavy_workday" say "$rig" "\$host" "\$id" "\$@"
EOF
    chmod +x "$rig/bin"/*
}

# workday_cleanup: stops the window, hosts and models, and removes the rig
# unless WORKDAY_KEEP=1.
workday_cleanup() {
    local host file pid pids=()
    [[ -n "${rig:-}" ]] || return 0
    workday_way "$session" record stop >/dev/null 2>&1 || true
    workday_way "$session" stop >/dev/null 2>&1 || true
    for host in lab ci gpu-box; do
        for file in "$rig/$host/host.pid" "$rig/$host/model.pid"; do
            if [[ -f "$file" ]]; then
                pids+=("$(cat "$file")")
            fi
        done
    done
    # A host that `bin/host start` brought back is not this shell's child,
    # so `wait` does not cover it: it must be gone before the rig is.
    for pid in ${pids[@]+"${pids[@]}"}; do
        kill "$pid" 2>/dev/null || continue
        for _ in {1..100}; do
            kill -0 "$pid" 2>/dev/null || continue 2
            sleep .05
        done
        kill -9 "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    if [[ "${WORKDAY_KEEP:-0}" == 1 ]]; then
        printf 'kept the rig: %s\n' "$rig" >&2
    else
        rm -rf -- "$rig"
    fi
}
