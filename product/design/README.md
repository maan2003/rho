# Built rho renders

The thread and idea captures show the leading draft and newest-first mail; the full Desk captures show only what waits. The runtime captures show the project’s automatic choice on laptop and a draft override on phone. Other screens were captured from `9256e944` in rho's headless Wayland compositor with bundled Rho OKSolar P3. Laptop: 1300×850 at scale 1. Phone: 390×844 logical pixels (780×1688 at scale 2). Ages, clock times, the fixture directory, and the agent ID vary; these are not pixel goldens.

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

Open the palette with `:`, choose `new project`, and enter `tide@lab`. Make an
empty directory with `mkdir "$rig/tide"`; enter that path at `tide@lab works on:`.
Known places, such as `$rig/repo` after the clock agent starts, appear as
completions. Accept `one` at `tide@lab agents:` and `opus-high` at `tide@lab runtime:`; Desk opens narrowed to
`tide@lab · on tide · one agent · opus-high`, with `0 wait`. At phone width (including a
second local device), open `tide@lab` by name and `form_shot phone empty-project`.

For the `per idea` project capture below, first open `clock@lab` by name
on either window. In its palette choose `agents: one`, then select `per idea`
(Down, Enter). The narrowed Desk updates its heading; the running agent and
the existing waiting thread stay. `works on…` shows the present place and
lets you replace it without moving that agent.

### Show the project runtime and an idea override

In the narrowed `clock@lab` Desk, choose `runtime…` in the palette and set
`sol-high`. The Desk heading includes the new runtime. Open `i` for the
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
| [Mail body search](laptop-prompt.png) | From Desk, `form_way laptop input 'text::' 'wait:400' 'text:mail: behavior' 'wait:600'; form_shot laptop prompt`. |
| [Verdict menu below the card](laptop-menu.png) | `form_way laptop input 'key:space' 'wait:700'; form_shot laptop menu` |
| [Agent transcript at the mail's writing step](laptop-agent.png) | `form_way laptop input 'key:escape' 'wait:550' 'key:g' 'key:g' 'key:j' 'key:enter' 'wait:850'; form_shot laptop agent` |
| [Done echo back on Desk](laptop-echo.png) | `form_way laptop input 'key:ctrl+k' 'wait:600' 'key:space' 'wait:350' 'key:d' 'wait:320'; form_shot laptop echo` |

The echo says `nothing waits` when the last card is cleared. Use `space` then `u` to undo the verdict, then `form_way laptop stop`.

## Phone: project, card, palette, verdict, and agent

Run `form_gui phone` with no identity argument. Wait for the focused 390×844 logical window, and leave it unpaired. Pointer coordinates below use the 780×1688 capture size. Run the first three rows in order.

| Render | Capture |
| --- | --- |
| [Narrow Desk (per idea)](phone-project.png) | From Desk, `form_way phone input 'text::' 'wait:380' 'text:clock@lab' 'wait:480' 'key:enter' 'wait:800'; form_shot phone project`. The first result is the project. The project opens a narrow Desk. |
| Verdict sheet over the unpaired card | Tap the project's **open**, then the card's **more**: `form_way phone click 390 1640; sleep .55; form_way phone click 702 1640; sleep .55`. |
| [Blank palette over Desk](phone-prompt.png) | `form_way phone input 'key:escape' 'wait:350' 'key:tab' 'wait:400' 'text::' 'wait:600'; form_shot phone prompt`. |
| [Command-only palette](phone-palette-commands.png) | From the blank palette, `form_way phone input 'text:> note' 'wait:550'; form_shot phone palette-commands`. Only commands appear, without shortcut hints. |
| [Text-only palette](phone-palette-text.png) | From the command-only palette, `form_way phone input 'key:escape' 'wait:320' 'text::' 'wait:280' 'text:/ behavior' 'wait:550'; form_shot phone palette-text`. Tap the mail row to open its matching line. |

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

Seed a disposable thread with March history, today's cold arrival, a user reply,
and a subsequent warm ask. The seed prints the thread ID (2 on a new fixture).

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
form_way "$device" input key:i 'text:Keep the reference clock.' \
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

The [open thread](laptop-reading.png) puts the draft above the warm mail. Today's
cold mail has a recap before its body; March and warm mail don't.
On [laptop](laptop-reading-notice.png) and [phone](phone-reading-notice.png), the
stuck draft starts at the same place as it did inline. The gutter runs to the
seam beneath the last line; the notice uses that reserved space without moving
the draft or the history.
Use `gg` to return to the draft inline and clear the notice.
While there, run `"$fixture" edit "$rig/runtime" "$ws" "$worker" "$mid" $'Both checks pass.\n\nNo retries were changed.\n\nBoth clocks use the same input.'`:
history grows below the draft, but the draft and caret stay put.

### A warm mail whose whole body is a recap

In the same fixture, send a user reply from the open thread. Then send project
mail with `"$fixture" send` and edit it to `## recap`, a short paragraph,
a blank line, and another paragraph with no intervening heading. Use `gg` to
read the newest mail. [Before](laptop-recap-only-before.png), the edited mail
vanished. [After](laptop-recap-only-after.png), it stays visible as ordinary
mail with its `edited` tag; the cold recap below remains accented.

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

## Inspect an outcome mail and a transcript with full commands

Run `FAKE_SCENARIO=real-tool-rounds FAKE_ARGS='--seed 899421 --rounds 2' bash product/design/capture.sh /tmp/rho-form-proof target/debug`.
Use **Put a clock idea to work** with `Run the clock acceptance checks.` for the idea. When the result waits, deal it with `space` and capture [proof](laptop-proof.png). Move `j` from the draft to the project's words, then press `enter` for the [transcript](laptop-agent.png). The command source, including each last shell argument, and the successful output are visible without opening a fold.

## Find a bookmark after closing its tab

Build `rho-browser --example fake_browser` and point `RHO_CUSTOM_BRAVE_BIN` at `target/debug/examples/fake_browser` in the fixture shell. Open `https://guide.example.invalid/clock#checks`, choose `bookmark this page…`, and name it `Acceptance guide`. Choose `close page` in the palette, stop the laptop window, and reuse that identity at phone width. Type `Acceptance guide` in the palette and capture with `form_shot phone bookmark`. Tapping the row opens the saved address in a fresh tab.

## What the renders expose

- The compact `unpaired` notice leaves the reason and age visible on the phone; a long subject gives way first. The same notice is absent when the laptop identity is reused.
- Desk shows the wait count, waiting subjects with their projects, and any overflow. Choosing a project narrows the same Desk and adds its host-owned place and mode to the top line.
- In a thread, the draft leads the newest-first mail; `reply` appears at its caret while it is empty. The thin left bar marks the draft and the user’s sent mail, not the project’s words. The card bar gives `done` and `snooze` their own buttons in Normal, and `undo` and `send` while writing. The [idea draft](laptop-idea.png) fills the window instead of adding a strip or tail.
- The palette can match mail body words absent from the subject. `>` limits rows to commands, `/` to mail and note text; `mail:` and `note:` retain their narrower scopes. Up and Down wrap the selection without changing the query. Blank on Desk it shows recently visited places other than waiting threads, followed by commands. Narrow command rows omit shortcuts.
- Both transcripts start with the brief and land on the mail's writing step. The phone shows the directory basename and wraps the step in the same reading area.

These renders cover the states named above, not pending or private delivery, host disconnection, a failed agent, alternate appearances, or motion timings.
