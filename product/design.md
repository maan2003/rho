# design

The built window puts the work in one editor. The top line tells the user where they are and what needs attention; questions and choices sit at the bottom. This is the built form, including Home and host chat. It does not specify a look for the future desk or project mail. Values are for the bundled dark appearance, Rho OKSolar P3; sizes are in logical pixels. The [laptop and phone renders](design/README.md) show the states and how to reproduce them.

## token color
- reading surface · `oklch(23.0% 0.018 150)` · the continuous field the user reads and writes on.
- reading text · `oklch(70.9% 0.012 95)` · the name or words the user came to read.
- quiet · `oklch(56.0% 0.012 100)` · context that can wait: ages, settled things, host tags, placeholders, and hints.
- attention · `oklch(63.1% 0.145 86.4)` · something to act on, here in the card's words or elsewhere as the lamp.
- lost connection · `oklch(63.1% 0.160 21.6)` · a host the user cannot reach.
- answer · `oklch(63.1% 0.195 244.8)` · a short explanation, including the answer to the last key.
- marked · `oklch(72.0% 0.110 105)` · text picked out from the reading line: the speaker, a link, or the key for a choice.
- keys in motion · `oklch(78.0% 0.120 185)` · a visible key or mode that tells the user how the keyboard will act.
- choice value · `oklch(66.0% 0.120 135)` · the value attached to a choice, not its name or key.
- point · `oklch(78.0% 0.012 95)` · where the user's attention is in the editor, and marks that guide the eye through text.
- current answer · `oklch(35.5% 0.022 150)` · the prompt row that Enter would take.
- seam · `oklch(30.5% 0.020 150)` · a quiet boundary between reading and controls, softened to 60% on the top line.
- command strip · reading surface with HSL lightness raised by `0.04` on the dark appearance · the bottom area that takes a question or a choice.
- behind the sheet · black at `35%` opacity · the screen is still there but the phone menu is taking the tap.

## token type
- editor text · Rho Font, `15 px`, weight `400` · one voice from the work to the prompt and top line.
- reading rhythm · `1.4` times the editor text size · next line visible without crowding the one in view.
- emphasis · weight `700` · make the speaker, unread name, menu title, or attached value findable without making it larger.
- private aside · italic · the user's own remark rather than a turn in the exchange.
- thumb icon · `18 px` · recognize the action before reading its word.
- thumb label · `11 px` · confirm which action the icon will take.

## token space
- top line · `26 px` · keep the heading and mode in a shallow fixed band.
- same band gap · `12 px` · separate the heading from its card words and separate the notices at the right.
- reading inset · `8 px` · keep the edge of window controls off the frame.
- seam width · `1 px` · divide the box, top line, and phone controls without adding a panel.
- composer breath · `4 px` above and below · make the place for writing distinct from the last message.
- command strip breath · `2 px` above and below · keep prompt and menu close to the text rhythm.
- thumb target · `56 px` high · an action the thumb can hit at the bottom edge or in a menu sheet.
- narrow window · up to `600 px` wide · make room for thumb controls rather than using a desktop menu grid.
- row air · two text spaces · leave space between a row name and its words.
- menu column · four rows, then the next column across · find choices without spending the whole screen's height.
- prompt answers · up to eight visible rows · keep the answer being considered in sight without making a second screen.
- box size · one to ten editor lines · use only the height of the words being written until a long draft needs to scroll.

## token time
- echo life · `4 s` · long enough to see the result of the last action before the heading returns.
- age beat · `1 min` · refresh elapsed ages on the thread and its list without a ticking clock on every row.
- card age step · nearest minute under an hour, tenth of an hour under a day, tenth of a day after that · change the words when the displayed age changes.
- result stays current · `3 days` · the built deal lowers an informational result until it goes away.
- while needed · event-bound, no fixed duration · the signal or open choice stays while it is true or still takes the next action.
- cut · immediate, no transition · hand the keys or the line to the newly active thing.
- thumb settle · `180 ms` · finish the phone page move or return it to where it was.

## part Status line
purpose: tell the user what fills the window, where the keys go, and whether anything needs attention elsewhere.
looks: a single `top line` in `editor text` over the `reading surface`, with a `seam` below. The name and any card words share the left; the lamp, host connection, pairing notice, and mode sit to the right with the `same band gap`.
states:
- ordinary: the screen's name in `reading text`, and the short mode word at the right; `NOR` is distinct from the warm `INS`.
- card open: the reason and age sit beside the heading in `attention`.
- attention elsewhere: a steady `attention` dot before the notices and mode; no dot when nothing elsewhere is important enough.
- host away: a named `lost connection` notice, or a count when more than one host is away.
- unpaired: a `quiet` notice that the verdicts stay on this device; on the phone it can displace the heading and card words.
- echo: the left side reads the `Echo line`; mode, lamp, and host warning keep their places.
used on: `window.Screen`, including the built Home, channel, and thread described by `desk.Desk`, `mail.Project`, and `mail.Thread`.
form:
```text
Flaky clock test    result · 8m                                  ●   NOR
```

## part Echo line
purpose: answer the action the user just took without opening anything.
looks: one line of `answer` in `editor text`, occupying the left of the `Status line`.
states:
- quiet: absent; the normal heading is in view.
- answer or refusal: the first line of the latest notice appears in the same place and color, then goes; an open prompt clears it.
used on: `window.Screen`, including `mail.Thread` and the built Home under `desk.Desk`.
form:
```text
done: Flaky clock test · nothing waits                                  NOR
```

## part Row
purpose: let the user pick a place, thread, or answer by name and see just enough context beside it.
looks: one row of `editor text`; name first in `reading text`, then the quieter context. A channel row can wrap on the phone. Home pads names into a column and keeps `row air` before words; the channel uses `row air` before counts and centered dots between them. A prompt leaves a `reading inset` and gives the picked answer the `current answer` fill.
states:
- Home: a room name, then its host and, if anything waits, the words before the host; a card takes `attention`, a snooze or ordinary unread count takes `quiet`.
- channel: a thread name, with `emphasis` for unread, `attention` for mentions or a stopped agent, then reply counts, participants, and age as available.
- prompt or palette: the candidate's name and optional `quiet` explanation or key; the chosen row keeps its fill all the way across.
- empty: Home or a channel says there is nothing to open; a prompt with no answers shows just the input.
used on: built Home (`desk.Desk`), channel (`mail.Project`), and the command and find prompts (`window.Palette`).
form:
```text
  clock              result · 8m · lab
Flaky clock test  @1 · 2 new · 1 reply · 8m
Flaky clock test   clock · lab
```

## part Message header
purpose: identify who wrote the message and when, before the user reads its words.
looks: the name in `marked` with `emphasis`, then a centered dot and `quiet` time, on a line above the message in `editor text`.
states:
- delivered: `you`, the agent's name, or `system`, and the time.
- sending: the header reads `you · sending…` in `quiet` until the host has the message.
- private reply: `🔒 you ·` and time in `quiet` with `private aside`, like the words below it.
used on: built host thread (`mail.Thread`).
form:
```text
you · 09:12
Fix the clock test that fails one run in five.
```

## part Quote
purpose: let the user read the words another message is answering without mistaking them for new words.
looks: quoted Markdown keeps its `>` marker in the reading line. The words use `editor text` on the `reading surface`, without a separate quote panel.
states:
- plain: the body has no quote marker.
- quoted: `>` starts the quoted line; the reply can follow in ordinary text.
used on: built host thread (`mail.Thread`) and transcript (`agents.Agent`) when the writer includes quoted Markdown.
form:
```text
> Fix the clock test that fails one run in five.
A separate clock per test should do it.
```

## part Box
purpose: give the user somewhere to write without leaving the work they were reading.
looks: `editor text` below a `seam` with a `reading inset` and `composer breath`; it shares the reading surface and grows to `box size`.
states:
- empty thread: `quiet` says `message · ctrl-enter sends`.
- empty channel: `quiet` says `idea · ctrl-enter starts an agent · ctrl-shift-enter only sends`; the end can clip on a phone.
- writing: the cursor and draft replace the hint; the mode word says `INS`.
- after send: the box clears, the point returns to the messages, and the words on their way are marked in the history, not in the box.
used on: built channel (`mail.Project`) and host thread (`mail.Thread`).
form:
```text
message · ctrl-enter sends
```

## part Menu
purpose: offer a few keyed choices without taking the user away from the thing they are on.
looks: on a laptop an `emphasis` title and compact `menu column` grid sit in the `command strip` over the bottom of the work; `marked` keys lead each column. Available values in parentheses use `choice value`. On a phone `thumb target` rows start with their action, end with the key in `keys in motion`, and have `behind the sheet` over the work.
states:
- closed: the work fills its space, and no menu is visible.
- open: title, actions, keys, and any values; on the phone the sheet has `close` at the top.
- sub-menu: it takes the same place; the phone sheet adds `‹ back`.
- empty: no sheet or grid; the `Echo line` says `nothing here` with the menu name.
used on: `window.Screen` wherever `window.Menu` opens, including verdicts on the built `mail.Thread` and Home (`desk.Desk`).
form:
```text
Flaky clock test
  d  done       n  next card
  m  mute
  s  snooze
  u  undo
```

## part Prompt
purpose: ask for a line of text and offer answers without a dialog.
looks: `editor text` in the `command strip`, question and input first with a `reading inset`, then the candidate rows down the window. The input wraps below a long question on a narrow screen.
states:
- blank query: question and any initial answers.
- typing: text narrows the list; moving the selection keeps the `current answer` among `prompt answers`.
- no match: question and input only.
- closed: the strip goes away and the keys return.
used on: `window.Palette` and other questions over `window.Screen`, as in `hosts.Attach a host`.
form:
```text
command: start agent, choosing
start agent, choosing where and runtime…
start agent, choosing where and runtime… › new workset on a repository…  n
start agent, choosing where and runtime… › here, in a directory on the host…  h
```

## part Phone bar
purpose: give the thumb the actions that the current place can use.
looks: one `thumb target` band with a `seam` above and equally wide buttons; each `thumb icon` sits over its `thumb label`, both in `quiet`.
states:
- Home with a room: back, menu, and open; with a card, done, mute, snooze, and more too.
- reading elsewhere: back, home, menu, and more or send where those act on the work.
- prompt: only cancel and enter.
- menu sheet: the sheet takes taps in front of the work and bar.
used on: `window.Screen` at `narrow window` width, including the built Home, host thread, and `window.Palette`.
form:
```text
  ‹       ⌂       ☰       ⋯       ↑
 back    home    menu    more    send
```

## part Card's words
purpose: say why something wants the user and how long it has waited.
looks: a reason, centered dot, and age in `attention`, beside a room name in a `Row` and beside the heading in the `Status line`. On a phone the heading shortens before these words.
states:
- ask, result, or stop: the reason and the age, not just the color.
- snoozed room: the row says until when in `quiet` if no other card is waiting in it.
- nothing waiting: no card words; ordinary row information remains.
- echo: the words give way on the top line until the answer is gone.
used on: built Home (`desk.Desk`) and open card thread (`mail.Thread`).
form:
```text
asks · 8m      result · 1.5h      failed · 1.2d
snoozed · until 05:17
```

## motion An answer replaces the heading
means: the last action has an answer; the ordinary name and card words return without the user doing anything.
lasts: `echo life`; a new answer replaces the old one and a prompt takes it away.

## motion The lamp switches on or off
means: something important enough wants the user elsewhere, not the card already in view; the dot is steady, not a pulse.
lasts: `while needed`; at a higher crossing the chime sounds once, not on each refresh.

## motion The ages catch up
means: relative ages change in place as the work waits, without moving the user's point.
lasts: `age beat` on thread summaries; `card age step` for the deal's words.

## motion A result sinks
means: in the built deal, an informational result loses urgency and finally leaves the hand; the row is ordered again, not faded with an animation.
lasts: `result stays current`.

## motion A question or menu takes the keys
means: the bottom strip or sheet is present while it can take a choice; a laptop menu covers the work instead of reflowing it.
lasts: `cut` on open, replacement, or close; otherwise `while needed`.

## motion A phone page settles
means: a flick either turns the page or gives it back; the brief slide shows which happened.
lasts: `thumb settle`.

## rule the echo takes the name's place
when the `Echo line` answers, then the `Status line` gives up its heading and `Card's words`, not its lamp, connection warning, or mode.

## rule a room wears its waiting card's words
when a `Row` in Home has a waiting card, then `Card's words` take the place of the room's quiet waiting text rather than making a second row.

## rule a prompt takes the phone bar
when a `Prompt` opens on a phone, then the `Phone bar` offers its cancel and enter instead of the work's actions.

## rule the menu leaves the work where it is
when a `Menu` opens, then the `Row` or `Box` behind it keeps its point and the choices take the keys.
