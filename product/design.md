# design

The built window puts the work in one editor. The top line tells the user where they are and what needs attention; questions and choices sit at the bottom. This is the built form of Desk, the project list, mail, and the agent transcript. Values are for the bundled dark appearance, Rho OKSolar P3; sizes are in logical pixels. The [laptop and phone renders](design/README.md) show the states and how to reproduce them.

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
- seam width · `1 px` · divide the global idea strip, top line, and phone controls without adding a panel.
- composer breath · `4 px` above and below · give the global idea strip room to breathe.
- command strip breath · `2 px` above and below · keep prompt and menu close to the text rhythm.
- thumb target · `56 px` high · an action the thumb can hit at the bottom edge or in a menu sheet.
- narrow window · up to `600 px` wide · make room for thumb controls rather than using a desktop menu grid.
- row air · two text spaces · leave space between a row name and its words.
- menu column · four rows, then the next column across · find choices without spending the whole screen's height.
- prompt answers · up to eight visible rows · keep the answer being considered in sight without making a second screen.
- box size · one to ten editor lines · give the global idea strip only the height of the words being written until a long idea needs to scroll.

## token time
- echo life · `4 s` · long enough to see the result of the last action before the heading returns.
- age beat · `1 min` · refresh elapsed ages on the thread and its list without a ticking clock on every row.
- card age step · `now` during the first minute, nearest minute thereafter under an hour, tenth of an hour under a day, tenth of a day after that · change the words when the displayed age changes.
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
- unpaired: a `quiet` notice that verdicts stay on this device. On the phone it says `unpaired`, leaving the reason and age in view before the heading.
- echo: the left side reads the `Echo line`; mode, lamp, and host warning keep their places.
used on: `window.Screen`, including `desk.Desk`, `mail.Project`, `mail.Thread`, and `agents.Agent`.
form: [dealt clock thread on the laptop](design/laptop-card.png); [short heading on an unpaired phone](design/phone-menu.png); [agent with its host, folder and activity](design/laptop-agent.png).

## part Echo line
purpose: answer the action the user just took without opening anything.
looks: one line of `answer` in `editor text`, occupying the left of the `Status line`.
states:
- quiet: absent; the normal heading is in view.
- answer or refusal: the first line of the latest notice appears in the same place and color, then goes; an open prompt clears it.
used on: `window.Screen`, including `mail.Thread` and `desk.Desk`.
form: [done echo back on Desk](design/laptop-echo.png).

## part Row
purpose: let the user pick a place, thread, or answer by name and see just enough context beside it.
looks: one row of `editor text`; name first in `reading text`, then the quieter context. `desk.Desk` sets subjects, card words, and addresses in separate columns, with `row air` between columns; it never pads proportional text with spaces. The count uses the same face and is not a selectable row. A narrow Desk row puts its words below the subject; the project list can wrap its counts. The list uses `row air` before counts and centered dots between them. A prompt leaves a `reading inset` and gives the picked answer the `current answer` fill.
states:
- desk: a subject, card words in `attention` or status in `quiet`, and a project address in `quiet`; a folded row uses the address instead of a subject.
- project list: a thread name, with `emphasis` for unread, `attention` for mentions or a stopped agent, then mention, unread and reply counts, a status if present, and age.
- prompt or palette: the candidate's name, its `attention` card words or `quiet` status if present, then its `quiet` address or key; query matches use `marked`; the chosen row keeps its fill all the way across.
- empty: Desk reads `0 wait` when no cards are due; the project list reads `no threads yet`; the palette shows `no match` when no row fits the query.
used on: `desk.Desk`, `mail.Project`, and `window.Palette`.
form: [laptop Desk columns](design/laptop-desk.png), [phone Desk rows](design/phone-desk.png), [wrapped unread project row](design/phone-channel.png), and [matched palette row](design/laptop-prompt.png).

## part Message header
purpose: identify who wrote the message and when, before the user reads its words.
looks: the name in `marked` with `emphasis`, then a centered dot and `quiet` time, on a line above the message in `editor text`.
states:
- delivered: `you` or the project's address, and the time; an edited mail adds `edited`.
- sending: the header reads `you · sending…` in `quiet` until the host has the message.
- private reply: `🔒 you ·` and time in `quiet` with `private aside`, like the words below it.
used on: `mail.Thread`.
form: [you and clock@lab above their respective mails](design/laptop-card.png).

## part Quote
purpose: let the user read the words another message is answering without mistaking them for new words.
looks: quoted Markdown keeps its `>` marker in the reading line. The words use `editor text` on the `reading surface`, without a separate quote panel.
states:
- plain: the body has no quote marker.
- quoted: `>` starts the quoted line; a block of several lines starts folded to its first line and `…`. The reply can follow in ordinary text.
used on: built host thread (`mail.Thread`) and transcript (`agents.Agent`) when the writer includes quoted Markdown.
form:
```text
> Fix the clock test that fails one run in five.
A separate clock per test should do it.
```

## part Draft tail
purpose: let the user write or answer without leaving the buffer.
looks: one line in `quiet` after the last mail and any status (or the last Desk row), with a divider, the recipient, and the send key on laptops. The phone omits the shortcut. The draft is ordinary `editor text` immediately below it on the same reading surface.
states:
- empty: the separator stays visible with a blank editor line below it.
- writing: the cursor sits in the draft and the mode word says `INS`; on return to `NOR`, the cursor remains in the draft.
- after send: the new mail is above the separator, marked while it is on its way; the tail is empty. On Desk the echo says where it went.
used on: `desk.Desk`, `mail.Thread`, and the project list (`mail.Project`).
form: [laptop idea tail](design/laptop-desk.png), [laptop reply tail](design/laptop-card.png), and [short phone reply tail](design/phone-card.png).

## part Menu
purpose: offer a few keyed choices without taking the user away from the thing they are on.
looks: on a laptop an `emphasis` title and compact `menu column` grid sit in the `command strip` over the bottom of the work; `marked` keys lead each column. Available values in parentheses use `choice value`. On a phone `thumb target` rows start with their action, end with the key in `keys in motion`, and have `behind the sheet` over the work.
states:
- closed: the work fills its space, and no menu is visible.
- open: title, actions, keys, and any values; on the phone the sheet has `close` at the top.
- sub-menu: it takes the same place; the phone sheet adds `‹ back`.
- empty: no sheet or grid; the `Echo line` says `nothing here` with the menu name.
used on: `window.Screen` wherever `window.Menu` opens, including verdicts in `mail.Thread` and `agents.Agent`. Desk does not offer verdicts.
form: [laptop verdict grid](design/laptop-menu.png) and [phone verdict sheet](design/phone-menu.png).

## part Prompt
purpose: ask for a line of text and offer answers without a dialog.
looks: `editor text` in the `command strip`, question and input first with a `reading inset`, then the candidate rows down the window. The input wraps below a long question on a narrow screen.
states:
- blank query: question and any initial answers.
- typing: text narrows the list; moving the selection keeps the `current answer` among `prompt answers`.
- no match: `no match` below the input when a choice is required and none fits.
- closed: the strip goes away and the keys return.
used on: `window.Palette` and other questions over `window.Screen`, as in `hosts.Attach a host`.
form: [filtered laptop palette](design/laptop-prompt.png) and [blank phone palette](design/phone-prompt.png).

## part Phone bar
purpose: give the thumb the actions that the current place can use.
looks: one `thumb target` band with a `seam` above and equally wide buttons; each `thumb icon` sits over its `thumb label`, both in `quiet`.
states:
- desk: menu, open, and more; open deals the top card and a tap on a row opens that row.
- card open: back, done, snooze, and more; send is added where there is a draft. Verdicts land on the card, including from the agent transcript that belongs to it.
- reading elsewhere: back, desk, and menu; send is added where there is a draft.
- writing an idea: cancel and send.
- prompt: only cancel and enter.
- menu sheet: the sheet takes taps in front of the work and bar.
used on: `window.Screen` at `narrow window` width, including `desk.Desk`, `mail.Thread`, `agents.Agent`, and `window.Palette`.
form: [Desk](design/phone-desk.png), [card with send](design/phone-card.png), [transcript without send](design/phone-agent.png), and [palette](design/phone-prompt.png).

## part Card's words
purpose: say why something wants the user and how long it has waited.
looks: a reason, centered dot, and age in `attention`, beside a subject in a `Row` and beside the heading in the `Status line`. On a phone the heading shortens before these words.
states:
- ask, result, or stop: the reason and the age, not just the color.
- snoozed thread: it leaves the Desk waiting rows until it can be dealt again.
- nothing waiting: no card words; ordinary row information remains.
- echo: the words give way on the top line until the answer is gone.
used on: `desk.Desk` and `mail.Thread` when open as a card.
form: [ask on the phone's top line](design/phone-card.png) and [grouped results on Desk](design/laptop-desk.png).

## motion An answer replaces the heading
means: the last action has an answer; the ordinary name and card words return without the user doing anything.
lasts: `echo life`; a new answer replaces the old one and a prompt takes it away.

## motion The lamp switches on or off
means: something important enough wants the user elsewhere, not the card already in view; the dot is steady, not a pulse.
lasts: `while needed`; at a higher crossing the chime sounds once, not on each refresh.

## motion The ages catch up
means: relative ages change in place as the work waits, without moving the user's point.
lasts: `age beat` on thread summaries; `card age step` for the deal's words.

## motion A question or menu takes the keys
means: the bottom strip or sheet is present while it can take a choice; a laptop menu covers the work instead of reflowing it.
lasts: `cut` on open, replacement, or close; otherwise `while needed`.

## motion A phone page settles
means: a flick either turns the page or gives it back; the brief slide shows which happened.
lasts: `thumb settle`.

## rule the echo takes the name's place
when the `Echo line` answers, then the `Status line` gives up its heading, `Card's words`, and pairing notice, not its lamp, connection warning, or mode.

## rule a thread wears its waiting card's words
when a `mail.Thread` has a waiting card, then its `Row` on `desk.Desk` carries `Card's words` in the waiting section and does not also appear under `moving`.

## rule a prompt takes the phone bar
when a `Prompt` opens on a phone, then the `Phone bar` offers its cancel and enter instead of the work's actions.

## rule the menu leaves the work where it is
when a `Menu` opens, then the `Row` or `Draft tail` behind it keeps its point and the choices take the keys.
