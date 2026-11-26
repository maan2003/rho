#!/usr/bin/env bash
# Boot the heavy-workday rig in a named rho wayland session for exploratory
# QA, print how to drive it, and keep it running until this script ends.
# See README.md.
set -euo pipefail
here=$(dirname "$(realpath "${BASH_SOURCE[0]}")")
# shellcheck source=product/design/workday.sh
source "$here/workday.sh"
if [[ -z "${WORKDAY_NETNS:-}" && -z "${1:-}" ]]; then
    workday_build
fi
workday_isolate "$0" "$@"
workday_init "${1:-}"
if [[ -n "${QA_SESSION:-}" ]]; then
    session=$QA_SESSION
fi
trap workday_cleanup EXIT
trap 'exit 130' INT TERM
workday_seed
workday_boot
workday_gui "$session"
workday_helpers
workday_log "ready"
cat <<EOF

Heavy workday is running. Ctrl-C, or killing PID $$, stops it and removes
the rig. Every helper below works from any shell or agent.

  session      $session
  rig          $rig
  device       $rig/device: the window's XDG directories and gui.log
  lab          host PID $(cat "$rig/lab/host.pid"), model PID $(cat "$rig/lab/model.pid"), log $rig/lab/host.log
  ci           host PID $(cat "$rig/ci/host.pid"); its model is gone, so its agents retry, then stick
  gpu-box      away; no host runs, the device holds its log
  ids          $rig/fixture.json, $rig/ci-day.json

Drive the window (this build's rho, which can record):
  $rig/bin/way screenshot --output /tmp/shot.png
  $rig/bin/way input 'key:space' 'key:f' 'wait:300' 'text:watermark'
  $rig/bin/way key 'escape g g'
  $rig/bin/way record start --output /tmp/flow.webm
  $rig/bin/way record stop

Change the world:
  $rig/bin/host kill lab        # lab drops, as a crash does
  $rig/bin/host start lab       # lab comes back
  $rig/bin/say lab/flaky 'A new message for the long thread.'
  $rig/bin/say lab/flaky 'Still rebasing.' flash
  $rig/bin/inside COMMAND...    # anything else, inside the rig's network
EOF
if [[ -n "${QA_READY_FILE:-}" ]]; then
    jq -n --arg session "$session" --arg rig "$rig" --arg bins "$bins" \
        --argjson lab "$(cat "$rig/lab/host.pid")" --argjson ci "$(cat "$rig/ci/host.pid")" \
        --argjson pid $$ \
        '{session: $session, rig: $rig, bins: $bins, pid: $pid, hosts: {lab: $lab, ci: $ci},
          way: "\($rig)/bin/way", inside: "\($rig)/bin/inside"}' \
        >"$QA_READY_FILE"
fi
while workday_way "$session" status >/dev/null 2>&1; do
    sleep 2
done
printf 'the window closed\n' >&2
