# Built rho renders

The thread and idea captures show the leading draft and newest-first message; the full Desk captures show only what waits. The runtime captures show the project’s automatic choice on laptop and a draft override on phone. Other screens were captured from `9256e944` in rho's headless Wayland compositor with bundled Rho OKSolar P3. Laptop: 1300×850 at scale 1. Phone: 390×844 logical pixels (780×1688 at scale 2). Ages, clock times, the fixture directory, and the agent ID vary; these are not pixel goldens.

## The heavy workday: capture matrix and QA sessions

`capture-matrix.sh` and `qa-session.sh` put the real `rho-gui` in the middle of a
messy eight-hour day. The `heavy_workday` example
(`agent-host/rho-agent-host/examples/heavy_workday.rs`) writes every store in
about half a second. Then real hosts and the window start on that state:

| Host | What runs | What it holds |
| --- | --- | --- |
| `lab` | `rho-agent-host` and `rho-fake-model` (`clarifying-question`, reports an allowance) | Projects `tide`, `rho` and `infra`, with 203 messages. Waiting asks, three `infra` stops (stuck, failed, lost), a refused start, a muted and a snoozed ask, 26 settled threads, Claude allowance on `work` (74% left) and `home` (12% left), and 30 days of spend. The 20-message `Flaky clock test` has code blocks, a quote, PR links, an edited message, a fresh flash, a private reply and a draft. |
| `ci` | A host whose fake model (`real-tool-rounds`) is stopped during the day | Project `nightly`. The checks thread runs real commands and sends its proof, then the agent retries a model that is gone (`working`, then `stuck` after three minutes). The bisect thread is `stopped by you`. |
| `gpu-box` | Nothing: it is away | Projects `tide` and `train`, from the device's copy of its log. So `tide@lab` and `tide@gpu-box` collide. |

The device is paired. It holds 15 notes, the private reply, the mute, the snooze and the done verdicts.

Both scripts build what they need with one cargo run, unless you give a binary directory.
To build by hand:

```sh
cargo build -p rho-agent-host --example heavy_workday \
    -p rho-gui -p rho-inference -p rho-fake-model -p rho-cli --bins
```

The scripts need `rho-agent-desktop` on `PATH`, `jq`, `git`, and `unshare` with
unprivileged user namespaces. Each run has its own rig under `$TMPDIR` and removes it at the end.
`WORKDAY_KEEP=1` keeps the rig. The rig runs in a network namespace that has only loopback, as your user.
The hosts and the window use iroh, which otherwise reaches n0's relays and DNS. In the rig these calls fail (the window log shows `pkarr` errors).
`WORKDAY_NETWORK=host` turns the namespace off.

### Capture matrix

```sh
product/design/capture-matrix.sh /tmp/workday-captures [BIN_DIRECTORY]
```

It boots the rig at 1300×850, scale 1, and walks fixed keystroke journeys.
A run takes about 55 seconds when the binaries are built: 0.6 to seed, 5.5 to boot in all, and the rest for journeys. The cargo check adds about 7 seconds when nothing changed.
It writes these files:

| File | Journey |
| --- | --- |
| `desk-full-overflow-away.png` | The first Desk: 7 rows, `and 6 more`, `gpu-box away` |
| `thread-long-asking.png` | `space f`, `flaky clock test`, `enter` |
| `thread-stuck-draft-new.png` | `ctrl-d` three times, then an agent posts: the draft stays at the edge and `↑ new` shows |
| `transcript-errors.png` | The `ci` checks thread; `/exited`, `;`, `enter` on the agent's message: brief, commands, `retrying ×N` |
| `usage-7d.png` | `:usage`, `enter` |
| `notes-list.png` | `:list notes`, `enter` |
| `places-mru.png` | `space f` with no query: the places just visited |
| `places-fulltext-match.png` | `ManualClock` matches messages and a note |
| `flow-deal-and-places.webm` | Desk, `space-j`, `space f` `watermark`, `enter`, `tab`. The script sends `ctrl-j`, because `rho wayland` cannot send `f20`, the key that `space-j` makes. |
| `commands-kill-aliases.png` | In the long thread, `:kill` |
| `commands-inline-arg.png` | `:model sol-medium` |
| `idea-draft.png` | `ctrl-alt-m`, `ctrl-t` `tide@lab`, the words |
| `idea-live-reply.png` | After `ctrl-enter`, the agent's question in the new thread |
| `flow-commands-and-idea.webm` | The four captures above, as one recording |
| `manifest.json` | For each capture: the screen, its state, the keys that led to it, the check, its SHA-256 and its time. Also the commit, the hosts, the thread ids and the timings. |

The script stops with an error, and shows the end of the window log, when one of these occurs:

- `rho-gui` exits, panics or loses its window. The script examines this before each capture.
- A capture is blank: fewer than 16 colors, or less than 0.2% of it differs from the background.
- A capture is the same frame as the capture before it. Each step changes the screen, so the window has stopped drawing.
- A recording has no VP8/VP9 video track, no keyframe, or fewer than 10 frames.
- A key step, a screenshot or a wait does not finish in time.

`heavy_workday check-png FILE [PREVIOUS]` and `check-webm FILE` make these checks, and you can run them on any capture.

### QA sessions

```sh
QA_SESSION=qa QA_READY_FILE=/tmp/qa.json product/design/qa-session.sh [BIN_DIRECTORY]
```

It boots the same rig and window, prints the session, the rig, the host and model PIDs, and the helper commands. Then it waits until you stop it (Ctrl-C) or until the window closes.
`QA_READY_FILE` gets a JSON object with the same facts when the rig is ready. The helpers in
`$rig/bin` work from any shell or agent, inside or outside the namespace:

| Helper | Does |
| --- | --- |
| `way ARG...` | `rho wayland` on the window, from this build, so `record` works |
| `host kill lab` / `host start lab` | Drops a host as a crash does, or starts it again on the same store. `ci` works too. |
| `say lab/flaky TEXT [flash]` | A new agent posts in a seeded thread. `fixture.json` names the threads. Agent text must pass the host's Simplified Technical English check. |
| `inside COMMAND...` | Runs anything in the rig's network, for example `heavy_workday py "$rig" lab CODE` to read the host's chat in Python |

Ages, clock times, ids and the `reconnecting` timer change from run to run. The captures are not pixel goldens.

## Run the fixture

In the repo's dev shell, build the GUI, host, worker, and fake model. Use `/src/cargo-one` in the shared workspace; elsewhere the same arguments work with `cargo`. Pass the directory with the four binaries to the fixture.

```sh
/src/cargo-one build --locked --offline -p rho-gui -p rho-agent-host -p rho-inference -p rho-fake-model --bins
FAKE_ARGS='--seed 51' bash product/design/capture.sh /tmp/rho-form-captures target/debug
```

The script opens a shell with `form_gui`, `form_way`, and `form_shot`. The host, the two device identities, and the model are temporary; the only attached host is the local fixture `lab`. Exit the shell to stop the windows and discard the fixture. The local worker needs the normal agent workset prerequisites.

`FAKE_SCENARIO` defaults to `clarifying-question`. `FAKE_ARGS` is an optional whitespace-separated argument list (no shell expansion); `rho-fake-model --help` lists the choices. Terminal faults are disabled. The fixture policy translates the default Claude runtime to `sol-medium` on `lab` only. The real worker runs against the fake model without Claude's mount overlays.

For the Chart, add `--report-allowance` to `FAKE_ARGS` to have the model report
66% left with a reset in five days on a Responses WebSocket turn. Leave it off
to see spend without an allowance. Before any work, the spending charts are empty.

### Put a clock idea to work

Run these steps in the fixture shell, on either scenario. After `form_gui`, use `form_way laptop tree` and wait for the focused `rho-gui` window. Dismiss the synthetic device phrase with Enter; do not capture the phrase.

```sh
form_gui laptop
form_way laptop tree
# Once the window and its first prompt are ready:
form_way laptop input 'key:enter' 'wait:750' 'key:i' 'wait:600' \
    'text:Fix the clock test that fails one run in five.' 'wait:450' 'key:ctrl+enter' \
    'wait:600' 'text:clock@lab' 'wait:500'
form_shot laptop idea
form_way laptop input 'key:enter' 'wait:450' "text:$rig/repo" 'wait:400' \
    'key:enter' 'wait:450'
form_shot laptop project-mode
form_way laptop input 'key:enter' 'wait:550'
form_shot laptop project-runtime
form_way laptop input 'key:enter' 'wait:800'
```

The [address chooser](laptop-idea.png) offers `new project clock@lab`; Enter
takes that row. After `clock@lab works on:`, the [mode prompt](laptop-project-mode.png)
starts on `one`. After it, accept `opus-high` (marked `default`) at the [runtime prompt](laptop-project-runtime.png) to send the idea and return to Desk. For the [same chooser at phone width](phone-idea.png), start an idea on the unpaired phone, type its words, open the address chooser with `ctrl-t`, then type `foam@lab`. The chooser also accepts a tap on the row. On the default scenario, wait until the clock ask is visible
on Desk.

### Start an empty project and change the next idea's mode

Open the palette with `:`, choose `project › new`, and enter `tide@lab`. Make an
empty directory with `mkdir "$rig/tide"`; enter that path at `tide@lab works on:`.
Known places, such as `$rig/repo` after the clock agent starts, appear as
completions. Accept `one` at `tide@lab agents:` and `opus-high` at `tide@lab runtime:`; the echo reads
`created project: tide@lab`, and the palette row for `tide@lab` reads `on tide · one agent · opus-high`. At phone width (including a
second local device), type `tide@lab` in the palette and `form_shot phone empty-project`.

For the `per idea` project capture below, choose `project › agents…` in the
palette on either window, answer `project:` with `clock@lab`, then select `per idea`
(Down, Enter). The project's palette row updates; the running agent and
the existing waiting thread stay. `project › works on…` shows the present place and
lets you replace it without moving that agent. From a `clock@lab` thread these
commands skip the `project:` question.

### Show the project runtime and an idea override

Choose `project › runtime…` in the palette, answer `project:` with `clock@lab`, and set
`sol-high`. The project's palette row includes the new runtime. Open `i` for the
[automatic choice on laptop](laptop-idea-empty.png); `on:` shows `sol-high`
without adding it to the draft. Use `form_shot laptop idea-empty`. For the
[explicit choice on phone](phone-idea-runtime.png), move from the words to
`to:` and then `on:` with Shift-Tab twice. Press Enter and choose `sol-medium`,
then return to the words with Shift-Tab and type `Write this on the chosen
runtime.` Use `form_shot phone idea-runtime` before sending. Changing `to:`
updates an automatic runtime but leaves an explicit choice alone.

## Laptop: card, palette, verdict, and agent

Starting from the waiting clock ask, capture the card, verdict, and agent in order. Capture the palette separately from Desk. `space` deals on Desk when a card waits and shows the verdicts in the thread. After clearing the last card, `space` on Desk offers undo.

| Render | Capture |
| --- | --- |
| [Dealt clock thread](laptop-card.png) | `form_way laptop input 'key:g' 'key:g' 'wait:400' 'key:space' 'wait:800'; form_shot laptop card` |
| [Message body search](laptop-prompt.png) | With the seeded clock history (`FORM_SEED_THREAD=1`), from Desk, `form_way laptop input 'text::' 'wait:400' 'text:chat: steady' 'wait:600'; form_shot laptop prompt`. |
| [Verdict menu below the card](laptop-menu.png) | `form_way laptop input 'key:space' 'wait:700'; form_shot laptop menu` |
| [Agent transcript at the message's writing step](laptop-agent.png) | `form_way laptop input 'key:escape' 'wait:550' 'key:g' 'key:g' 'key:j' 'key:enter' 'wait:850'; form_shot laptop agent` |
| [Done echo back on Desk](laptop-echo.png) | `form_way laptop input 'key:ctrl+k' 'wait:600' 'key:space' 'wait:350' 'key:d' 'wait:320'; form_shot laptop echo` |

The echo says `nothing waits` when the last card is cleared. Use `space` then `u` to undo the verdict, then `form_way laptop stop`.

## Phone: project, card, palette, verdict, and agent

Run `form_gui phone` with no identity argument. Wait for the focused 390×844 logical window, and leave it unpaired. Pointer coordinates below use the 780×1688 capture size. Run the first three rows in order.

| Render | Capture |
| --- | --- |
| [Palette filtered to a project (per idea)](phone-project.png) | From Desk, `form_way phone input 'text::' 'wait:380' 'text:clock@lab' 'wait:480' 'key:enter' 'wait:800'; form_shot phone project`. The first result is the project. Enter fills `clock@lab ` and the palette lists its threads. |
| Verdict sheet over the unpaired card | Open the waiting thread, then tap the card's **more**: `form_way phone input 'key:enter' 'wait:800'; form_way phone click 702 1640; sleep .55`. |
| [Blank palette over Desk](phone-prompt.png) | `form_way phone input 'key:escape' 'wait:350' 'key:tab' 'wait:400' 'text::' 'wait:600'; form_shot phone prompt`. |
| [Command-only palette](phone-palette-commands.png) | From the blank palette, `form_way phone input 'text:> note' 'wait:550'; form_shot phone palette-commands`. Only commands appear, without shortcut hints. |
| [Text-only palette](phone-palette-text.png) | From the command-only palette, `form_way phone input 'key:escape' 'wait:320' 'text::' 'wait:280' 'text:/ behavior' 'wait:550'; form_shot phone palette-text`. Tap the message row to open its matching line. |

For the card without the `unpaired` notice, reuse the laptop identity:

```sh
form_way phone stop
form_gui phone laptop
form_way phone tree
# Wait for the restored clock thread. Tab goes to Desk; tap open to deal.
form_way phone input 'key:tab' 'wait:600'
form_way phone click 390 1640
sleep .7
form_shot phone card-empty
# Tap the project's words to inspect their writing step.
form_way phone click 165 164
sleep .75
form_shot phone agent
```

This makes the phone card and the [phone transcript](phone-agent.png). The transcript carries the same verdicts as the thread but has no `send` action. To capture a [card with `send`](phone-card.png), return to the card, tap the draft (`form_way phone click 95 77`), and type `Use a separate clock for each test.` without sending it. For the paired verdict sheet, press Escape to leave Insert mode, then tap **more** (`form_way phone click 702 1640`), pause, and `form_shot phone menu`.

## Read long history while drafting

Seed a disposable thread with March history, today's report, a user reply,
and a subsequent ask. The seed prints the thread ID (2 on a new fixture).

```sh
/src/cargo-one build --locked --offline -p rho-agent-host --example thread_reading_fixture
FORM_SEED_THREAD=1 bash product/design/capture.sh /tmp/rho-form-thread target/debug
# Run once per device, in a fresh fixture shell each time.
device=laptop                         # or phone
form_gui "$device"
# Wait for the fresh clock fixture window to receive keys.
for _ in {1..20}; do
    form_way "$device" tree | jq -e \
        '[.Windows[] | select(.app_id == "rho-gui" and .is_focused)] | length == 1' >/dev/null && break
    sleep .2
done
sleep .7
form_way "$device" input key:enter wait:950 key:space wait:700
form_shot "$device" reading
form_way "$device" input key:i 'text:Keep this clock.' \
    key:enter 'text:Both counters can use it.' key:enter 'text:Leave the retry alone.' \
    key:enter 'text:Log the chosen clock.' key:enter 'text:Run both tests twice.' \
    key:enter 'text:Compare the timestamps.' key:enter 'text:Keep the earlier result.' \
    key:enter 'text:Send the exact output.' wait:550
sleep 1
form_way "$device" input key:escape key:g key:g wait:650
form_way "$device" input 'key:ctrl+d' 'wait:550'
fixture="$bins/examples/thread_reading_fixture"
read -r ws worker mid < <("$fixture" send "$rig/runtime" "$rig/repo" 2 \
    $'Both checks pass.\n\nThe retry policy did not change.')
sleep .6
form_shot "$device" reading-notice
```

The [open thread](laptop-reading.png) puts the draft above the newest message.
The earlier messages read in place.
On [laptop](laptop-reading-notice.png) and [phone](phone-reading-notice.png), the
stuck draft starts at the same place as it did inline. The gutter runs to the
seam beneath the last line; the notice uses that reserved space without moving
the draft or the history.
Use `gg` to return to the draft inline and clear the notice.
While there, run `"$fixture" edit "$rig/runtime" "$ws" "$worker" "$mid" $'Both checks pass.\n\nNo retries were changed.\n\nBoth clocks use the same input.'`:
history grows below the draft, but the draft and caret stay put.

### Send a reply from a dealt card

With the long thread fixture, send a quiet newer message with the `send`
command above, then open the kept ask from the Desk. The draft has no
addressee even though a different message appears below it first. Tap the
draft, type `Keep the monotonic timer.`, and tap **send**. The reply
[appears in the shared thread](phone-thread-wide-reply.png) and the dealt ask
no longer waits. Another window on the same host sees the reply and the
settlement from the log.

## Fresh flashes in the thread and the Desk

Start the same disposable host with `FORM_FLASH_LEAD_SECONDS=120` as well
as `FORM_SEED_THREAD=1`. Four flashes are logged before a newer kept ask;
the host expires them at the deadline printed by the fixture. The short
lead is set only by seeding earlier original post times, not by switching
the clock or bypassing the running host.

- Open `laptop` with `form_gui`; dismiss the synthetic phrase, then
  [`form_shot laptop flash-desk-before`](laptop-flash-desk-before.png). The kept ask is the only card; its
  row includes the newest flash's first line.
- Open the card with Space; [`form_shot laptop flash-thread-before`](laptop-flash-thread-before.png). Both
  writers have a quiet flash in the history, with their original ages.
  Move to `+2 earlier` and press Enter (or tap the line on `phone`) to show
  those two [earlier flashes](phone-flash-expanded.png) in their original positions; repeat to fold them again.
- After the printed deadline, [`form_shot laptop flash-thread-after`](laptop-flash-thread-after.png),
  then Tab to Desk and [`form_shot laptop flash-desk-after`](laptop-flash-desk-after.png). The flashes
  and quiet Desk words are gone without re-opening the thread; the same
  kept ask still waits.
- To send a flash from a live fixture workset, run
  `"$bins/examples/thread_reading_fixture" flash "$rig/runtime" "$rig/repo" 2 'I am checking the second clock.'`.
  It appears without a card. It is now the latest item, so time
  alone will not remove it; something newer must also arrive.

## Desk on laptop and phone: what waits

In a fresh fixture shell on the default `clarifying-question` scenario, follow
**Put a clock idea to work**. Once its question waits, send eight more ideas
to `clock@lab`. From Desk, `i` opens the idea editor with the last project
already in its address field.

```sh
for idea in \
    'Check cache eviction after the clock fix.' \
    'Check retries without changing the clock.' \
    'Check midnight rollover on a slow machine.' \
    'Does the timer survive a sleep and wake cycle?' \
    'Keep metrics timestamps in UTC.' \
    'Measure skew after the network comes back.' \
    'Reproduce the slow test on two cores.' \
    'Write up the deadline tolerance.'
do
    form_way laptop input 'key:i' 'wait:450' "text:$idea" \
        'wait:420' 'key:ctrl+enter' 'wait:1100'
done
```

When the Desk reads `9 wait` and the sent notice has gone, select the first
row with `gg` and capture. Stop the laptop before reusing its identity at
phone width.

```sh
form_way laptop key 'g g'
form_shot laptop desk
form_way laptop stop
form_gui phone laptop
form_way phone tree
# Wait for the nine cards to load, then select the first row.
form_way phone key 'g g'
form_shot phone desk
```

Both captures show seven subjects with project addresses, then
`and 2 more · clock@lab`. Enter or a tap on that overflow opens
“Reproduce the slow test on two cores.” To check the empty case with work
in flight, start a separate `FAKE_SCENARIO=slow-trickle` fixture and send
only the first idea. Both Desk and `clock@lab` show `0 wait` until there
is a card.

## Inspect an outcome message and a transcript with full commands

Run `FAKE_SCENARIO=real-tool-rounds FAKE_ARGS='--seed 899421 --rounds 2' bash product/design/capture.sh /tmp/rho-form-proof target/debug`.
Use **Put a clock idea to work** with `Run the clock acceptance checks.` for the idea. When the result waits, deal it with `space` and capture [proof](laptop-proof.png). Move `j` from the draft to the project's words, then press `enter` for the [transcript](laptop-agent.png). The command source, including each last shell argument, and the successful output are visible without opening a fold.

## Find a bookmark after closing its tab

Build `rho-browser --example fake_browser` and point `RHO_CUSTOM_BRAVE_BIN` at `target/debug/examples/fake_browser` in the fixture shell. Open `https://guide.example.invalid/clock#checks`, choose `bookmark this page…`, and name it `Acceptance guide`. Choose `close page` in the palette, stop the laptop window, and reuse that identity at phone width. Type `Acceptance guide` in the palette and capture with `form_shot phone bookmark`. Tapping the row opens the saved address in a fresh tab.

## What the renders expose

- The compact `unpaired` notice leaves the reason and age visible on the phone; a long subject gives way first. The same notice is absent when the laptop identity is reused.
- Desk shows the wait count, waiting subjects with their projects, and any overflow. Choosing a project in the palette fills its name, so the palette lists that project's threads; the project's row shows its host-owned place, mode, and runtime.
- In a thread, the draft leads the newest-first message; `reply` appears at its caret while it is empty. The thin left bar marks the draft and the user’s sent message, not the project’s words. The card bar gives `done` and `snooze` their own buttons in Normal, and `undo` and `send` while writing. The [idea draft](laptop-idea.png) fills the window instead of adding a strip or tail.
- The palette can match message body words absent from the subject. `>` limits rows to commands, `/` to message and note text; `chat:` and `note:` retain their narrower scopes. Up and Down wrap the selection without changing the query. Blank on Desk it shows recently visited places other than waiting threads, followed by commands. Narrow command rows omit shortcuts.
- Both transcripts start with the brief and land on the message's writing step. The phone shows the directory basename and wraps the step in the same reading area.

These renders cover the states named above, not pending or private delivery, host disconnection, a failed agent, alternate appearances, or motion timings.
