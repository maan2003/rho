# Built rho renders

The card, Desk, and idea captures were updated 2026-11-20. Other screens were captured from `9256e944` in rho's headless Wayland compositor with bundled Rho OKSolar P3. Laptop: 1300×850 at scale 1. Phone: 390×844 logical pixels (780×1688 at scale 2). Ages, clock times, the fixture directory, and the agent ID vary; these are not pixel goldens.

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
    'wait:600' 'text:clock' 'wait:500' 'key:tab' 'wait:450' 'key:enter'
form_shot laptop idea
# The project is new: h chooses “here, in a directory on the host…”.
form_way laptop input 'key:ctrl+enter' 'wait:800' 'key:h' 'wait:450' \
    "text:$rig/repo" 'wait:400' 'key:enter'
```

The address chooser's entry is `new project clock@lab`; `tab` selects it explicitly before `enter` creates the project. On the default scenario, wait until the clock ask is visible on Desk (the work may briefly appear under `moving`).

## Laptop: card, palette, verdict, and agent

Starting from the waiting clock ask, run the rows in order. `space` deals on Desk when a card waits and shows the verdicts in the thread. After clearing the last card, `space` on Desk offers undo.

| Render | Capture |
| --- | --- |
| [Dealt clock thread](laptop-card.png) | `form_way laptop input 'key:g' 'key:g' 'wait:400' 'key:space' 'wait:800'; form_shot laptop card` |
| [Mail body search](laptop-prompt.png) | `form_way laptop input 'text::' 'wait:400' 'text:mail: behavior' 'wait:600'; form_shot laptop prompt` |
| [Verdict menu over the card](laptop-menu.png) | `form_way laptop input 'key:escape' 'wait:600' 'key:space' 'wait:700'; form_shot laptop menu` |
| [Agent transcript at the mail's writing step](laptop-agent.png) | `form_way laptop input 'key:escape' 'wait:550' 'key:enter' 'wait:850'; form_shot laptop agent` |
| [Done echo back on Desk](laptop-echo.png) | `form_way laptop input 'key:ctrl+k' 'wait:600' 'key:space' 'wait:350' 'key:d' 'wait:320'; form_shot laptop echo` |

The echo says `nothing waits` when the last card is cleared. Use `space` then `u` to undo the verdict, then `form_way laptop stop`.

## Phone: project, card, palette, verdict, and agent

Run `form_gui phone` with no identity argument. Wait for the focused 390×844 logical window, and leave it unpaired. Pointer coordinates below use the 780×1688 capture size. Run the first three rows in order.

| Render | Capture |
| --- | --- |
| [Project with wrapped unread row](phone-project.png) | From Desk, `form_way phone input 'text::' 'wait:380' 'text:clock@lab' 'wait:480' 'key:enter' 'wait:800'; form_shot phone project`. The first result is the project. The project opens a narrow Desk. |
| [Verdict sheet over the card](phone-menu.png) | Tap the project's **open**, then the card's **more**: `form_way phone click 390 1640; sleep .55; form_way phone click 702 1640; sleep .55; form_shot phone menu`. |
| [Blank palette over Desk](phone-prompt.png) | `form_way phone input 'key:escape' 'wait:350' 'key:tab' 'wait:400' 'text::' 'wait:600'; form_shot phone prompt`. |

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
form_way phone click 165 203
sleep .75
form_shot phone agent
```

This makes the phone card and the [phone transcript](phone-agent.png). The transcript carries the same verdicts as the thread but has no `send` action. To capture a [card with `send`](phone-card.png), return to the card, tap anywhere below the project’s words, and type a reply without sending it before taking the screenshot.

## Desk on laptop and phone: what moves

Start a separate fixture shell with the slow model:

```sh
FAKE_SCENARIO=slow-trickle FAKE_ARGS='--seed 51' \
    bash product/design/capture.sh /tmp/rho-form-desk target/debug
```

Repeat **Put a clock idea to work**, then send a second idea to the same project from Desk. `i` opens the idea editor with the last project already in its address field.

```sh
form_way laptop input 'key:i' 'wait:450' \
    'text:Check cache eviction after the clock fix.' 'wait:400' 'key:ctrl+enter'
```

Send the third idea and capture while it is still `working`. Stop the laptop before reusing its identity at phone width.

```sh
form_way laptop input 'key:i' 'wait:450' \
    'text:Check retries without changing the clock.' 'wait:400' 'key:ctrl+enter' \
    'wait:4700' 'key:g' 'key:g' 'wait:430'
form_shot laptop desk
form_way laptop stop
form_gui phone laptop
form_way phone tree
# Wait for the Desk and its rows to load, before the third result arrives.
form_shot phone desk
```

The [laptop Desk](laptop-desk.png) and [phone Desk](phone-desk.png) show work under `moving`, with only subject, project, and latest words.

## What the renders expose

- The compact `unpaired` notice leaves the reason and age visible on the phone; a long subject gives way first. The same notice is absent when the laptop identity is reused.
- Desk shows waiting subjects and their projects. Moving rows add the latest project words or `working`. Choosing a project narrows the same Desk.
- In a thread, the draft is the next mail; `reply` appears while it is empty. The card bar gives `done` and `snooze` their own buttons in Normal, and `undo` and `send` while writing. The [idea draft](laptop-idea.png) fills the window instead of adding a strip or tail.
- The palette can match mail body words absent from the subject. Blank on Desk it shows recently visited places other than waiting threads, followed by commands. Narrow command rows omit shortcuts.
- Both transcripts start with the brief and land on the mail's writing step. The phone shows the directory basename and wraps the step in the same reading area.

These renders cover the states named above, not pending or private delivery, host disconnection, a failed agent, alternate appearances, or motion timings.
