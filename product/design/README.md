# Built rho renders

Captured 2026-11-20 from `d147b654` with rho's headless Wayland compositor and bundled Rho OKSolar P3. Laptop: 1300×850 at scale 1. Phone: 390×844 logical pixels (780×1688 at scale 2). The ages, time of day, and agent name are generated; the captures are not pixel goldens. Later host mail changes shortened the `result` label and added `system` as a sender; these renders predate those changes.

## Run the fixture

In the repo’s dev shell, build the GUI, local host, worker, and fake model.
Pass the directory with the four built binaries if it is not `target/debug`.

```sh
cargo build --locked --offline -p rho-gui -p rho-agent-host -p rho-inference -p rho-fake-model --bins
bash product/design/capture.sh /tmp/rho-form-captures target/debug
```

The script opens a shell with `form_gui`, `form_way`, and `form_shot`. The host,
two client states, and model use temporary paths; the model listens on loopback.
The only attached host, `lab`, is this fixture. Exit the shell to discard the
fixture. The local worker needs the normal agent workset prerequisites.

**Start a working agent from the desk.** Open the laptop (`form_gui laptop`),
dismiss the synthetic device phrase with Enter, and press `i`. Type a project
name such as `tide` and choose `new project tide@lab`. Write the idea and press
`ctrl-enter`. The first idea for a project asks where its agents work; choose
`here, in a directory on the host…` (`h`) and enter the fixture’s `$rig/repo`.
The fixture host uses a temporary policy that translates the normal Claude
default to `sol-medium`: the real worker can then run against the fake model
without requiring Claude’s mount overlays. This policy only applies to `lab`;
there is no change to the product’s default runtime. The idea is still
posted and starts an agent through the normal application flow.

To control what the agent does, pass `FAKE_SCENARIO` and optional whitespace-
separated `FAKE_ARGS` (no shell expansion). For example, to keep the agent
working long enough to see it in `moving` before it replies:

```sh
FAKE_SCENARIO=slow-trickle FAKE_ARGS='--seed 51' \
    bash product/design/capture.sh /tmp/rho-moving target/debug
```

The default is `clarifying-question`. Run `target/debug/rho-fake-model --help`
for the available scenarios and flags. Injected terminal faults are disabled.
For the historical Home captures below, use the original checkout at
`d147b654` and select `sol-medium` explicitly when prompted.

```sh
form_gui laptop
form_way laptop tree        # wait for a focused 1300×850 rho-gui window
form_way laptop key enter   # dismiss the initial synthetic device phrase without capturing it
sleep 1
form_way laptop input 'text::' 'wait:450'
form_way laptop input 'text:new channel' 'wait:300' 'key:enter'
sleep .7
form_way laptop input 'text:clock' 'wait:300' 'key:enter'
sleep 1
# Creating a channel opens it on current builds. On d147b654, open clock from Home here.
sleep .5
form_way laptop input 'key:i' 'wait:300' \
  'text:Fix the clock test that fails one run in five.' 'wait:300' 'key:ctrl+shift+enter'
sleep 1
form_way laptop key enter   # open the new thread
sleep .6
form_way laptop input 'key:i' 'wait:350' \
  'text:> Fix the clock test that fails one run in five.' 'key:enter' \
  'text:A separate clock per test should do it.' 'wait:350' 'key:ctrl+enter'
sleep 2
form_way laptop input 'text::' 'wait:450'
form_way laptop input 'text:start agent, choosing' 'wait:450'
form_shot laptop prompt     # laptop-prompt.png
```

Confirm the prompt's first suggestion, choose `here, in a directory on the host…`, and choose the native runtime below. The fake agent sets the thread's status and sends an `ask` card. If needed, wait for the ask before capturing any card state.

```sh
form_way laptop input 'key:enter' 'wait:650' 'key:h' 'wait:650'
form_way laptop input "text:$rig/repo" 'wait:350' 'key:enter' 'wait:650'
form_way laptop input 'text:sol-medium' 'wait:350' 'key:enter'
form_way laptop key tab
sleep .5
form_way laptop input 'text::' 'wait:450'
form_way laptop input 'text:new channel' 'wait:450' 'key:enter'
sleep .7
form_way laptop input 'text:notes' 'wait:350' 'key:enter'
```

## Laptop

On Home in Normal mode, select the waiting `clock` row (press `up` if `notes` is selected). The ordinary `notes` row is beneath it:

| Render | Command |
| --- | --- |
| [Home with verdicts](laptop-menu.png) | `form_way laptop input 'key:space' 'wait:300'; form_shot laptop menu` |
| [Thread open as a card](laptop-card.png) | `form_way laptop input 'key:n' 'wait:700'; form_shot laptop card` |
| [Verdict echo](laptop-echo.png) | `form_way laptop input 'key:space' 'wait:350' 'key:d' 'wait:150'; form_shot laptop echo` |
| [Thread with filtered prompt](laptop-prompt.png) | `form_shot laptop prompt` at the filtered prompt in the fixture steps above |

The verdict returns to Home. Observe that its echo goes away without another key. To restore the card before using the laptop identity on the phone, run `form_way laptop input 'key:shift+u' 'wait:500' 'key:tab'`.

## Phone

Run `form_gui phone` to launch a fresh fixture identity. Wait for its window and dismiss the synthetic device phrase if shown. Leave this identity unpaired. From Home on the waiting `clock` row (use `tab` to return to Home if needed):

| Render | Command |
| --- | --- |
| [Channel with wrapped unread row](phone-channel.png) | `form_way phone input 'key:enter' 'wait:700'; form_shot phone channel` |
| [Home with verdict sheet](phone-menu.png) | `form_way phone input 'key:tab' 'wait:300' 'key:space' 'wait:350'; form_shot phone menu` |
| [Thread with unfiltered prompt](phone-prompt.png) | `form_way phone input 'key:n' 'wait:700' 'text::' 'wait:450'; form_shot phone prompt` |

For the card on the original fixture identity, stop both windows, then reopen that identity at phone width. From Home, pull the restored card:

```sh
form_way phone stop
form_way laptop stop
form_gui phone laptop
# Wait for the 390×844 logical window and the clock row on Home.
form_way phone input 'key:space' 'wait:250' 'key:n' 'wait:700'
form_shot phone card       # phone-card.png
```

## What the renders expose

- Phone Home / channel / thread · unpaired · Status line · the quiet pairing notice takes the whole name and, on the open card, the card words; the end of the notice is also missing · the user should still see where they are and why the card was dealt · `quiet`, `attention`, `Status line`.
- Home · waiting and ordinary rows · Row · the `lab` host goes from quiet to amber on the row with `result`; the eye reads provenance as part of the reason · only the waiting words should claim attention · `attention`, `quiet`.
- Phone channel · empty composer · Box · the hint ends after `starts an agent ·`; the second way to send falls out of view · the user should be able to read both send paths · `box size`, `quiet`.
- Phone thread · unfiltered palette · Row · the two long start commands are cut off where their distinguishing words and shortcuts would appear · the user should see how the options differ before choosing · `Row`, `Prompt`.
- Home · verdict menu · Menu · keys switch from warm at the start of a laptop row to cyan at the far edge of a phone row · a shortcut should read as the same kind of choice on both · `marked`, `keys in motion`.

No captures of pending or private delivery, disconnected hosts, asks/stops/snoozes, or alternate appearances. They are described only where the built renderer defines them. Chime and phone settle timings are source-derived, not measured recordings.
