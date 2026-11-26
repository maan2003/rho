#!/usr/bin/env bash
# The capture matrix: boots the heavy-workday rig (workday.sh), walks fixed
# keystroke journeys through the real rho-gui at 1300x850, and writes the
# PNGs, two WebM flows and manifest.json to OUTPUT_DIRECTORY. It fails when
# the window crashes, hangs or draws a blank frame. See README.md.
set -euo pipefail
here=$(dirname "$(realpath "${BASH_SOURCE[0]}")")
# shellcheck source=product/design/workday.sh
source "$here/workday.sh"
out=$(realpath -m "${1:?usage: capture-matrix.sh OUTPUT_DIRECTORY [BIN_DIRECTORY]}")
if [[ -z "${WORKDAY_NETNS:-}" && -z "${2:-}" ]]; then
    workday_build
fi
workday_isolate "$0" "$@"
workday_init "${2:-}"
trap workday_cleanup EXIT
trap 'exit 130' INT TERM
started=$(date +%s%N)

fail() {
    printf 'capture-matrix: %s\n' "$*" >&2
    if [[ -f "$rig/device/gui.log" ]]; then
        printf -- '--- the window log ends:\n' >&2
        tail -n 20 "$rig/device/gui.log" | cut -c1-400 >&2 || true
    fi
    exit 1
}

way() {
    timeout 30 "$bins/rho" wayland --state-dir "$rig/wayland" --session "$session" "$@"
}

# alive: the window's process runs, has not panicked and still has its
# window.
alive() {
    kill -0 "$gui_pid" 2>/dev/null || fail "rho-gui (PID $gui_pid) exited"
    if grep -q 'panicked at' "$rig/device/gui.log"; then
        fail "rho-gui panicked"
    fi
    way tree | jq -e --argjson pid "$gui_pid" \
        '[.Windows[] | select(.pid == $pid)] | length == 1' >/dev/null ||
        fail "rho-gui (PID $gui_pid) has no window"
}

# keys STEP...: input steps, as `rho wayland input` takes them; they are
# kept for the next capture's manifest entry.
pending=()
keys() {
    way input "$@" >/dev/null || fail "the window did not take: $*"
    pending+=("$@")
}

# shot NAME STATE: captures the screen to NAME.png and checks it against
# the capture before it. STATE says what it shows, for the manifest.
previous=
shot() {
    local name=$1 state=$2 file="$out/$1.png" check
    sleep "${SHOT_SETTLE:-0.25}"
    alive
    way screenshot --output "$file" >/dev/null || fail "no screenshot for $name"
    check=$("$bins/examples/heavy_workday" check-png "$file" ${previous:+"$previous"} 2>&1) ||
        fail "$name: ${check:-the check failed}"
    previous=$file
    record_entry png "$name" "$state" "$check"
    workday_log "$name.png $(jq -c '{colors, ink: (.ink * 1000 | round / 1000), changed: (if .changed then (.changed * 1000 | round / 1000) else null end)}' <<<"$check")"
}

record_entry() {
    local kind=$1 name=$2 state=$3 check=$4 file
    file="$name.$kind"
    jq -nc --arg file "$file" --arg kind "$kind" --arg screen "${name%%-*}" \
        --arg state "$state" --argjson check "$check" \
        --arg sha256 "$(sha256sum "$out/$file" | cut -d' ' -f1)" \
        --argjson at_ms "$(( ($(date +%s%N) - started) / 1000000 ))" \
        '{file: $file, kind: $kind, screen: $screen, state: $state, keys: $ARGS.positional,
          check: $check, sha256: $sha256, at_ms: $at_ms}' \
        --args ${pending[@]+"${pending[@]}"} >>"$rig/manifest.jsonl"
    pending=()
}

# settle: waits until the window stops changing, as hosts connect and
# their cards arrive. check-png prints its JSON even when it exits 1 on
# an identical frame (changed == 0.0), so check colors/ink as well so a
# blank frame never counts as settled.
settle() {
    local last="$rig/settle-0.png" next="$rig/settle-1.png" report
    way screenshot --output "$last" >/dev/null
    for _ in {1..40}; do
        sleep .25
        way screenshot --output "$next" >/dev/null
        report=$("$bins/examples/heavy_workday" check-png "$next" "$last" 2>/dev/null || true)
        if jq -en --argjson r "${report:-null}" \
            '$r != null and $r.colors >= 16 and $r.ink >= 0.002 and $r.changed < 0.0005' \
            >/dev/null; then
            return 0
        fi
        mv "$next" "$last"
    done
    fail "the window did not settle"
}

# record NAME STATE: starts the recording of a flow; `recorded` ends and
# checks it.
recording=
record() {
    recording=$1
    recording_state=$2
    way record start --output "$out/$1.webm" >/dev/null || fail "could not record $1"
    sleep .5
}

recorded() {
    local check
    sleep .8
    way record stop >/dev/null || fail "the recording of $recording did not stop"
    check=$("$bins/examples/heavy_workday" check-webm "$out/$recording.webm" 2>&1) ||
        fail "$recording: ${check:-the check failed}"
    record_entry webm "$recording" "$recording_state" "$check"
    workday_log "$recording.webm $(jq -c '{frames, duration_ms, bytes}' <<<"$check")"
}

# until_py HOST CODE: polls Python on HOST until it prints `yes`.
until_py() {
    for _ in {1..60}; do
        if [[ "$("$bins/examples/heavy_workday" py "$rig" "$1" "$2" 2>/dev/null | tail -n 1)" == yes ]]; then
            return 0
        fi
        sleep .5
    done
    fail "timed out waiting on $1 for: $2"
}

mkdir -p "$out"
for name in desk-full-overflow-away thread-long-asking thread-stuck-draft-new \
    transcript-errors usage-7d notes-list places-mru places-fulltext-match \
    commands-kill-aliases commands-inline-arg idea-draft idea-live-reply \
    flow-deal-and-places flow-commands-and-idea manifest; do
    rm -f "$out/$name".{png,webm,json}
done
workday_seed
workday_boot
workday_gui "$session"
gui_pid=$(way tree | jq -er '[.Windows[] | select(.app_id == "rho-gui")][0].pid')
booted=$(date +%s%N)
settle

shot desk-full-overflow-away \
    "Desk: 7 card rows then 'and N more'; gpu-box away with its cards from the device's copy; tide@lab beside tide@gpu-box"

keys 'key:space' 'key:f' 'wait:300' 'text:flaky clock test' 'wait:400' 'key:enter' 'wait:900'
shot thread-long-asking \
    "tide@lab › Flaky clock test waits: draft on top, a fresh flash, code blocks, a quote, PR links, an edited message, a private reply"

keys 'key:ctrl+d' 'key:ctrl+d' 'key:ctrl+d' 'wait:300'
"$bins/examples/heavy_workday" say "$rig" lab "$(jq -r '.threads["lab/flaky"].id' "$rig/fixture.json")" \
    'I pushed the legacy-clock flag. The release note is in the PR description.' >/dev/null ||
    fail "lab did not take the new message"
pending+=('say lab/flaky: I pushed the legacy-clock flag.' 'wait:1500')
sleep 1.5
shot thread-stuck-draft-new \
    "Scrolled back in the long thread: the draft stays at the edge, and '↑ new' marks a message that came in"

keys 'key:space' 'key:f' 'wait:300' 'text:nightly acceptance checks' 'wait:400' 'key:enter' 'wait:900' \
    'text:/' 'text:exited' 'key:enter' 'wait:200' 'text:;' 'key:enter' 'wait:1200'
shot transcript-errors \
    "ci's checks agent, opened with enter on its message: brief, three commands, then its model gone and the retries folded"

keys 'key:escape' 'text::' 'wait:300' 'text:usage' 'wait:400' 'key:enter' 'wait:1000'
shot usage-7d \
    "Usage, rate limit over 7 days: opus/lab/home at 12%, opus/lab/work at 74%, fable on both, gpt per host"

keys 'text::' 'wait:300' 'text:list notes' 'wait:400' 'key:enter' 'wait:900'
shot notes-list "Notes: fifteen notes and the private reply on the flaky thread, newest first"

keys 'key:space' 'key:f' 'wait:600'
shot places-mru "Places with no query: the places just visited, most recent first"

keys 'text:ManualClock' 'wait:700'
shot places-fulltext-match "Places for 'ManualClock': threads, notes and the words of messages across hosts"
keys 'key:escape' 'key:escape' 'key:tab' 'wait:500'

record flow-deal-and-places \
    "Desk, space-j deals the next card, space f finds 'watermark' in a message and jumps to it, tab is back on Desk"
keys 'key:ctrl+j' 'wait:1300' 'key:space' 'key:f' 'wait:500' 'text:watermark' 'wait:900' \
    'key:enter' 'wait:1300' 'key:tab' 'wait:1000'
alive
recorded

record flow-commands-and-idea \
    "Places jumps to the long thread; Commands: 'kill' finds stop agent, 'model sol-medium' sets the runtime inline; ctrl-alt-m writes an idea to tide@lab, sends it, and the agent's live question arrives"
keys 'key:space' 'key:f' 'wait:400' 'text:flaky clock test' 'wait:500' 'key:enter' 'wait:900' \
    'text::' 'wait:300' 'text:kill' 'wait:900'
shot commands-kill-aliases "Commands for 'kill' in the long thread: stop agent and cancel agent request answer the alias"
keys 'key:ctrl+u' 'text:model sol-medium' 'wait:900'
shot commands-inline-arg "Commands for 'model sol-medium' in the long thread: set project runtime takes tide@lab and the inline argument"
keys 'key:escape' 'key:escape' 'wait:300' 'key:ctrl+alt+m' 'wait:600' 'key:ctrl+t' 'wait:400' \
    'text:tide@lab' 'wait:400' 'key:enter' 'wait:400' \
    'text:Port the ManualClock fix to tide-sim and run the soak again.' 'wait:900'
shot idea-draft "An idea to tide@lab on sol-medium (to: and on:), before it is sent"
tide=$(jq -r '.threads["lab/tide"].id' "$rig/fixture.json")
keys 'key:ctrl+enter' 'wait:500'
until_py lab "from rho import chat
ideas = [m['id'] for m in chat.read($tide, 400) if m['text'].startswith('Port the ManualClock fix')]
print('yes' if ideas and any(m['sender'] != 'user' for m in chat.read(ideas[-1], 10)) else 'no')"
pending+=('wait until the agent replies')
keys 'key:space' 'key:f' 'wait:400' 'text:Port the ManualClock' 'wait:500' 'key:enter' 'wait:1200'
shot idea-live-reply "The sent idea's thread on tide@lab: the agent's live question waits"
recorded

alive
jq -n --slurpfile captures "$rig/manifest.jsonl" \
    --slurpfile fixture "$rig/fixture.json" --slurpfile ci_day "$rig/ci-day.json" \
    --arg commit "$(git -C "$here" rev-parse HEAD)" \
    --argjson dirty "$(if git -C "$here" diff --quiet HEAD; then echo false; else echo true; fi)" \
    --argjson boot_ms "$(( (booted - started) / 1000000 ))" \
    --argjson total_ms "$(( ($(date +%s%N) - started) / 1000000 ))" \
    --arg network "$(if [[ -n "${WORKDAY_NETNS:-}" ]]; then echo isolated; else echo host; fi)" \
    '{
        rig: "heavy workday",
        commit: $commit, dirty: $dirty, network: $network,
        window: {width: 1300, height: 850, scale: 1},
        hosts: {
            lab: "live, rho-fake-model clarifying-question; projects tide, rho, infra",
            ci: "live, its model stopped during the day; project nightly",
            "gpu-box": "away, never runs; projects tide, train from the device copy"
        },
        threads: $fixture[0].threads, endpoints: $fixture[0].endpoints, ci_day: $ci_day[0],
        timings_ms: {boot: $boot_ms, total: $total_ms},
        captures: $captures
    }' >"$out/manifest.json"
workday_log "wrote $(jq '.captures | length' "$out/manifest.json") captures and manifest.json to $out"
