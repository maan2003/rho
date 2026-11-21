# design

The built window puts the work in one editor. The top line tells the user where they are and what needs attention; questions and choices sit at the bottom. This is the built form of Desk, the project list, mail, and the agent transcript. Values are for the bundled dark appearance, Rho OKSolar P3; sizes are in logical pixels. The [laptop and phone renders](design/README.md) show the states and how to reproduce them.

## token color
- reading surface · `oklch(23.0% 0.018 150)` · the continuous field the user reads and writes on.
- reading text · `oklch(70.9% 0.012 95)` · the name or words the user came to read.
- quiet · `oklch(56.0% 0.012 100)` · context that can wait: ages, settled things, host tags, placeholders, and hints.
- attention · `oklch(63.1% 0.145 86.4)` · something to act on, here in the card's words or elsewhere as the lamp.
- lost connection · `oklch(63.1% 0.160 21.6)` · a host the user cannot reach.
- failed · `oklch(76.0% 0.140 25)` · a step that did not finish successfully.
- answer · `oklch(63.1% 0.195 244.8)` · a short explanation, including the answer to the last key.
- marked · `oklch(72.0% 0.110 105)` · text picked out from the reading line: the user's own mail in a thread, a link, a search match, or the key for a choice.
- recap · `oklch(73.0% 0.040 150)` · the part of a cold mail that lets the user answer without the history; a slight shift from reading text, not a second voice.
- keys in motion · `oklch(78.0% 0.120 185)` · a visible key or mode that tells the user how the keyboard will act, including `INS`.
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
- seam width · `1 px` · divide the top line and phone controls without adding a panel.
- command strip breath · `2 px` above and below · keep prompt and menu close to the text rhythm.
- stuck draft shadow · `4 px` down, `12 px` blur, black at `35%` · separate the draft from the history below its seam without dimming the draft.
- draft gutter stroke · `1 px` beside each inline line of a thread draft, `2 px` continuous when stuck · keep the left edge fixed while marking the whole stuck draft.
- thumb target · `56 px` high · an action the thumb can hit at the bottom edge or in a menu sheet.
- narrow window · up to `600 px` wide · make room for thumb controls rather than using a desktop menu grid.
- row air · two text spaces · leave space between a row name and its words.
- menu column · four rows, then the next column across · find choices without spending the whole screen's height.
- prompt answers · up to eight visible rows · keep the answer being considered in sight without making a second screen.

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
- ordinary: the screen's name in `reading text`, and the short mode word at the right; `INS` is in `keys in motion`.
- card open: `waits` and the age sit beside the heading in `attention`.
- attention elsewhere: a steady `attention` dot before the notices and mode; no dot when nothing elsewhere is important enough.
- host away: a named `lost connection` notice, or a count when more than one host is away.
- unpaired: a `quiet` notice that verdicts stay on this device. On the phone it says `unpaired`, leaving the reason and age in view before the heading.
- echo: the card's words give way to the `Echo line`; the heading, mode, lamp, and host warning keep their places.
used on: `window.Screen`, including `desk.Desk`, `mail.Project`, `mail.Thread`, and `agents.Agent`.
form: [dealt clock thread on the laptop](design/laptop-card.png); [short heading on the phone](design/phone-menu.png); [agent with its host, folder and activity](design/laptop-agent.png).

## part Echo line
purpose: answer the action the user just took without opening anything.
looks: one line of `answer` in `editor text`, after the heading in the `Status line`, where the card's words were.
states:
- quiet: absent; the normal heading is in view.
- answer or refusal: the first line of the latest notice appears in the same place and color, then goes; an open prompt clears it.
used on: `window.Screen`, including `mail.Thread` and `desk.Desk`.
form: [done echo back on Desk](design/laptop-echo.png).

## part Row
purpose: let the user pick a place, thread, or answer by name and see just enough context beside it.
looks: one row of `editor text`; name first in `reading text`, then the quieter context. On the laptop, `desk.Desk` puts `row air` between the subject and project address, or between a folded project's address and its stop count. It shortens the subject only when it does not fit in the width left after the row's own detail; it never pads proportional text with spaces. The count uses the same face and is not a selectable row. A narrow Desk row puts a folded project's stop count below its address; the project list can wrap its counts. The list uses `row air` before counts and centered dots between them. A prompt leaves a `reading inset` and gives the picked answer the `current answer` fill.
states:
- desk: a subject and a project address in `quiet`; a folded row of repeated stops uses the address in place of the subject and adds the count in `attention`.
- project list: a thread name, with `emphasis` for unread, `attention` for mentions or a stopped agent, then mention, unread and reply counts, a status if present, and age.
- prompt or palette: place rows use the Desk's columns with `row air` between them: subject, `attention` card words or the project's `quiet` latest words, and `quiet` address. On a narrow window the subject cuts at a word, the address stays on the first line, and the words move to the second. A command shows its name and its `quiet` key sequence, the name alone on a narrow window. Query matches use `marked`; the chosen row keeps its fill all the way across.
- empty: Desk reads `0 wait` when no cards are due; the project list reads `no threads yet`; the palette shows `no match` when no row fits the query.
used on: `desk.Desk`, `mail.Project`, and `window.Palette`.
form: [laptop Desk columns](design/laptop-desk.png), [phone Desk rows](design/phone-desk.png), [narrow Desk](design/phone-project.png), and [matched palette row](design/laptop-prompt.png).

## part Mail header
purpose: identify who wrote the mail and when, before the user reads its words.
looks: the name in `marked` with `emphasis`, then a centered dot and `quiet` time, on a line above the mail in `editor text`.
states:
- delivered: `you` or the project's address, and the time; an edited mail adds `edited`.
- sending: the header reads `you · sending…` in `quiet` until the host has the mail.
- private reply: `🔒 you ·` and time in `quiet` with `private aside`, like the words below it.
used on: `agents.Agent`. In `mail.Thread` the voice is told by colour, see `Mail`.
form: `you · 2m` or `clock@lab · 1m · edited` above the words; see `Mail` for the separate thread form.

## part Mail
purpose: let the user read an exchange and know who said what, with nothing between the mails but a blank line.
looks: the words in `editor text` on the `reading surface`; the user's own mail in `marked`, with a thin `marked` bar in the left gutter beside every line; the project's mail in `reading text` with no bar; one blank line between mails; a `quiet` name line above a mail from a third voice; a `quiet` tag at the end of the first line, such as `· edited` or `· taken back`; a `## recap` section in `recap`.
states:
- recap anchor: the heading and recap are drawn in `recap`, before the body of the mail.
- other mail: hide the recap unless it is the whole mail; then show the heading and words in regular mail colors.
- sending: the user's words in `quiet` until the host has the mail.
- private reply: `🔒 you ·` and time in `quiet` with `private aside`, like the words below it.
used on: `mail.Thread`.
form: [project and user mail on the laptop](design/laptop-card.png) and [the cold recap](design/laptop-reading.png).

## part Quote
purpose: let the user read the words another mail is answering without mistaking them for new words.
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

## part Draft
purpose: let the user write the next mail without leaving the buffer.
looks: the user's words in `marked`, like their sent mail, at the top of the thread, one blank line above the newest mail, on the same `reading surface`, with a thin `marked` bar in the left gutter beside every draft line (the `draft gutter stroke` in a thread), as the agent transcript draws the user's messages; nothing marks where the history begins but the user's own `marked` mail.
states:
- empty: a `quiet` placeholder where the words will go, `reply` in a thread and `idea` in a new draft, drawn in the text itself directly at the cursor with no gap, the way the old agent view drew `Write a message…`; the gutter bar is on that line too.
- stuck in a thread: the same text and gutter origin at the top edge, on the `command strip` surface. Below the last line only, leave half a `reading rhythm` plus `2 px` of air, then a `seam width` of `seam` and the `stuck draft shadow`. The `draft gutter stroke` runs through the air to the bottom of the seam. The band counts toward the half-window cap; `↑ new mail` appears in the reserved air, right-aligned in `quiet`, without moving the draft or history.
- answering an older mail: one line in `quiet` right under the draft's last line, `answers eng-ka35 · 12:40 · Which clock needs fixing?`, the first words of that mail cut at the window; absent when the draft answers the newest mail.
- writing: the cursor sits in the draft and the mode word says `INS`; on return to `NOR`, the cursor remains in the draft.
- after send: the words are the newest mail, right below the draft, marked while on their way; the draft is empty with the point in it and nothing scrolls. For an idea the echo says where it went.
used on: `mail.Thread` and the draft of `mail.Write an idea`.
form: [empty reply](design/laptop-card.png), [empty idea](design/laptop-idea-empty.png), and a stuck reply with new mail on [laptop](design/laptop-reading-notice.png) and [phone](design/phone-reading-notice.png).

## part Menu
purpose: offer a few keyed choices without taking the user away from the thing they are on.
looks: on a laptop an `emphasis` title and compact `menu column` grid sit in the `command strip` over the bottom of the work; `marked` keys lead each column. Available values in parentheses use `choice value`. On a phone `thumb target` rows start with their action, end with the key in `keys in motion`, and have `behind the sheet` over the work.
states:
- closed: the work fills its space, and no menu is visible.
- open: title, actions, keys, and any values; on the phone the sheet has `close` at the top and a full-width `cancel` at the bottom edge.
- sub-menu: it takes the same place; the phone sheet adds `‹ back`.
- empty: no sheet or grid; the `Echo line` says `nothing here` with the menu name.
used on: `window.Screen` wherever `window.Menu` opens, including verdicts in `mail.Thread` and `agents.Agent`. Desk does not offer verdicts.
form: [laptop verdict grid](design/laptop-menu.png) and [phone verdict sheet](design/phone-menu.png).

## part Prompt
purpose: ask for a line of text and offer answers without a dialog.
looks: `editor text` in the `command strip`, question and input first with a `reading inset`, then the candidate rows down the window. The input wraps below a long question on a narrow screen.
states:
- blank query: question and any initial answers.
- typing: text narrows the list; moving the selection keeps the `current answer` among `prompt answers`. The Palette shows the selected position and total in `quiet` at the right end of its prompt line.
- no match: `no match` below the input when a choice is required and none fits.
- closed: the strip goes away and the keys return.
used on: `window.Palette` and other questions over `window.Screen`, as in `hosts.Attach a host`.
form: [filtered laptop palette](design/laptop-prompt.png) and [blank phone palette](design/phone-prompt.png).

## part Phone bar
purpose: give the thumb the actions that the current place can use.
looks: one `thumb target` band with a `seam` above and equally wide buttons; each `thumb icon` sits over its `thumb label`, both in `quiet`.
states:
- desk: menu, open, and more; open deals the top card and a tap on a row opens that row.
- card open: back, menu, done, snooze, and more; while writing, undo and send take their place.
- agent transcript for an open card: back, done, snooze, more, and menu. Verdicts land on the card.
- reading elsewhere: back, desk, and menu; while writing, undo and send join.
- writing an idea: cancel, undo, and send.
- prompt: only cancel and enter.
- menu sheet: the sheet takes taps in front of the work, and its cancel target covers the bar.
used on: `window.Screen` at `narrow window` width, including `desk.Desk`, `mail.Thread`, `agents.Agent`, and `window.Palette`.
form: [Desk](design/phone-desk.png), [card with send](design/phone-card.png), [transcript without send](design/phone-agent.png), and [palette](design/phone-prompt.png).

## part Card's words
purpose: say how long something has waited for the user, or why the work stopped.
looks: `waits`, or a stop's word, a centered dot, and the age in `attention`, beside the heading in the `Status line` and on a thread row in the `Palette`. On a phone the heading shortens before these words.
states:
- waiting or stopped: the word and the age, not just the color.
- snoozed thread: it leaves the Desk waiting rows until it can be dealt again.
- nothing waiting: no card words; ordinary row information remains.
- echo: the words give way on the top line until the answer is gone.
used on: `mail.Thread` when open as a card, and `window.Palette`; not on `desk.Desk`, which is a glance.
form: [ask on the phone's top line](design/phone-card.png).

## part Allowance row
purpose: say how much an account has left and when it resets without making the user read the picture.
looks: one line of `editor text` per account above the picture, with `row air` before the share left and a centered dot before the reset time. The row uses its line's color, and `★` marks the GPT account the host is using.
states:
- reported: `★ gpt/work  66% left · resets in 5.0d`.
- no reset time: `gpt/home  18% left · reset time not reported`.
- reset time passed: `gpt/home  18% left · last report before reset`.
- no allowance from this account yet: `gpt/home  allowance not reported`.
used on: `usage.Chart`.
form:
```text
★ gpt/work  66% left · resets in 5.0d
gpt/home  18% left · resets in 2.1d
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

## motion A question or menu takes the keys
means: the bottom strip or sheet is present while it can take a choice; a laptop menu covers the work instead of reflowing it.
lasts: `cut` on open, replacement, or close; otherwise `while needed`.

## motion A phone page settles
means: a flick either turns the page or gives it back; the brief slide shows which happened.
lasts: `thumb settle`.

## rule the echo takes the card's words' place
when the `Echo line` answers, then the `Status line` gives up its `Card's words` and pairing notice; its heading, lamp, connection warning, and mode stay.

## rule a prompt takes the phone bar
when a `Prompt` opens on a phone, then the `Phone bar` offers its cancel and enter instead of the work's actions.

## rule the menu leaves the work where it is
when a `Menu` opens, then the `Row` or `Draft` behind it keeps its point and the choices take the keys.
