# Built rho renders

Captured 2026-11-20 from `9256e944` in rho's headless Wayland compositor with bundled Rho OKSolar P3. Laptop: 1300×850 at scale 1. Phone: 390×844 logical pixels (780×1688 at scale 2). Ages, clock times, the fixture directory, and the agent ID vary; these are not pixel goldens.

## Run the fixture

In the repo's dev shell, build the GUI, host, worker, and fake model. Use `/src/cargo-one` in the shared workspace; elsewhere the same arguments work with `cargo`. Pass the directory with the four binaries to the fixture.

```sh
/src/cargo-one build --locked --offline -p rho-gui -p rho-agent-host -p rho-inference -p rho-fake-model --bins
FAKE_ARGS='--seed 51' bash product/design/capture.sh /tmp/rho-form-captures target/debug
```

The script opens a shell with `form_gui`, `form_way`, and `form_shot`. The host, the two device identities, and the model are temporary; the only attached host is the local fixture `lab`. Exit the shell to stop the windows and discard the fixture. The local worker needs the normal agent workset prerequisites.

`FAKE_SCENARIO` defaults to `clarifying-question`. `FAKE_ARGS` is an optional whitespace-separated argument list (no shell expansion); `rho-fake-model --help` lists the choices. Terminal faults are disabled. The fixture policy translates the default Claude runtime to `sol-medium` on `lab` only. The real worker runs against the fake model without Claude's mount overlays.

### Put a clock idea to work

Run these steps in the fixture shell, on either scenario. After `form_gui`, use `form_way laptop tree` and wait for the focused `rho-gui` window. Dismiss the synthetic device phrase with Enter; do not capture the phrase.

```sh
form_gui laptop
form_way laptop tree
# Once the window and its first prompt are ready:
form_way laptop input 'key:enter' 'wait:500' 'key:i' 'wait:350' \
    'text:clock' 'wait:400' 'key:enter' 'wait:800' \
    'text:Fix the clock test that fails one run in five.' 'wait:400' 'key:ctrl+enter'
# The project is new: h chooses “here, in a directory on the host…”.
form_way laptop input 'wait:700' 'key:h' 'wait:450' "text:$rig/repo" 'wait:400' 'key:enter'
```

The address chooser's entry is `new project clock@lab`. On the default scenario, wait until the clock ask is visible on Desk (the work may briefly appear under `moving`).

## Laptop: card, palette, verdict, and agent

Starting from the waiting clock ask, run the rows in order. `space` deals on Desk; only once a card is open does `space` show its verdicts.

| Render | Capture |
| --- | --- |
| [Dealt clock thread](laptop-card.png) | `form_way laptop input 'key:g' 'key:g' 'wait:400' 'key:space' 'wait:800'; form_shot laptop card` |
| [Filtered palette over the thread](laptop-prompt.png) | `form_way laptop input 'text::' 'wait:400' 'text:start agent' 'wait:600'; form_shot laptop prompt` |
| [Verdict menu over the card](laptop-menu.png) | `form_way laptop input 'key:escape' 'wait:600' 'key:space' 'wait:700'; form_shot laptop menu` |
| [Agent transcript at the mail's writing step](laptop-agent.png) | `form_way laptop input 'key:escape' 'wait:550' 'key:enter' 'wait:850'; form_shot laptop agent` |
| [Done echo back on Desk](laptop-echo.png) | `form_way laptop input 'key:ctrl+k' 'wait:600' 'key:space' 'wait:350' 'key:d' 'wait:320'; form_shot laptop echo` |

The echo says `nothing waits` when the last card is cleared. Use `shift-u` to undo the verdict, then `form_way laptop stop`.

## Phone: project, card, palette, verdict, and agent

Run `form_gui phone` with no identity argument. Wait for the focused 390×844 logical window, and leave it unpaired. Pointer coordinates below use the 780×1688 capture size. Run the first three rows in order.

| Render | Capture |
| --- | --- |
| [Project with wrapped unread row](phone-project.png) | From Desk, `form_way phone input 'text::' 'wait:380' 'text:clock@lab' 'wait:480' 'key:enter' 'wait:800'; form_shot phone project`. The first result is the project. This is the existing project list, not the agreed Desk narrowed to a project. |
| [Verdict sheet over the card](phone-menu.png) | From the project, `form_way phone input 'key:tab' 'wait:550'`; tap Desk's **open**, then the card's **more**: `form_way phone click 390 1640; sleep .55; form_way phone click 702 1640; sleep .55; form_shot phone menu`. |
| [Blank palette over the card](phone-prompt.png) | `form_way phone input 'key:escape' 'wait:550' 'text::' 'wait:600'; form_shot phone prompt`. |

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
# The second mail header is the project's ask. Tap it to inspect the writing step.
form_way phone click 165 203
sleep .75
form_shot phone agent
```

This makes the phone card and the [phone transcript](phone-agent.png). The transcript carries the same verdicts as the thread but has no `send` action. To capture a [card with `send`](phone-card.png), return to the card, tap the draft separator, and type a reply without sending it before taking the screenshot.

## Desk on laptop and phone: waiting and moving together

Start a separate fixture shell with the slow model:

```sh
FAKE_SCENARIO=slow-trickle FAKE_ARGS='--seed 51' \
    bash product/design/capture.sh /tmp/rho-form-desk target/debug
```

Repeat **Put a clock idea to work**, then send a second idea to the same project from Desk. `i` enters the existing idea tail; no address prompt is needed.

```sh
form_way laptop input 'key:i' 'wait:450' \
    'text:Check cache eviction after the clock fix.' 'wait:400' 'key:ctrl+enter'
```

Wait until both results have arrived (roughly a minute; Desk reads `2 wait` and `2 results` on one `clock@lab` row). Send the third idea and capture while it is still `working`. Stop the laptop before reusing its identity at phone width.

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

The [laptop Desk](laptop-desk.png) and [phone Desk](phone-desk.png) show two results sharing one row and the third thread under `moving`. The addressed idea tail stays below the work.

## What the renders expose

- The compact `unpaired` notice leaves the reason and age visible on the phone; a long subject gives way first. The same notice is absent when the laptop identity is reused.
- Desk holds the card words in their own column on the laptop and below the subject on the phone. The project list is still a different screen, with its unread counts below the title when they wrap.
- The draft separator is part of the threaded mail or Desk buffer. On phones it names the recipient without the keyboard shortcut. The card bar gives `done` and `snooze` their own buttons; the Desk bar does not offer verdicts.
- The palette shows the matching name on the laptop and recent places before commands in the blank phone view. A menu's shortcut leads on the laptop and sits at the far edge on the phone.
- Both transcripts start with the brief and land on the mail's writing step. The phone shows the directory basename and wraps the step in the same reading area.

These renders cover the states named above, not pending or private delivery, host disconnection, a failed agent, alternate appearances, or motion timings.
