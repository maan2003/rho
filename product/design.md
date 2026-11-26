# design

The built window puts the work in one editor with sharp `0 px` square corners throughout (no rounded pills or cards), tuned primarily for a fullscreen 13" laptop display (`MacBook Air M2`, Asahi Linux + Niri, with a physical top-center camera notch). The top strip splits across the notch into a borderless **Left Ear** (belonging to the open page/card), a **square black center notch mask**, and a full-width framed **Right Ear** (the global status bar). Values are for the bundled dark appearance, Rho OKSolar P3; sizes are in logical pixels. The [laptop and phone renders](design/README.md) show the states and how to reproduce them.

## token color
- reading surface · `oklch(23.0% 0.018 150)` · the continuous field the user reads and writes on.
- global status surface · `oklch(20.6% 0.016 150)` · the subtle recessed field of the framed Right Ear global status bar.
- notch mask · `#000000` · solid square black rectangle framing the physical top-center notch so it reads as crisp square geometry.
- unread text · `oklch(89.5% 0.008 95)` · the newest unread agent turn and active page title.
- reading text · `oklch(83.5% 0.010 95)` · primary readable body text (`oklch(73.5% 0.010 95)` for already-read history).
- quiet · `oklch(56.0% 0.012 110)` · context that can wait: read ages, settled things, host tags, calm offline host notes on Desk, `NOR` mode, and hints.
- attention · `oklch(79.5% 0.145 78)` · something to act on: unread message age, pending push approval, or the waiting lamp.
- lost connection · `oklch(63.1% 0.160 21.6)` · a host the user cannot reach (`needs you`, or an `away` host required by the open screen).
- failed · `oklch(75.0% 0.145 25)` · a step that did not finish successfully.
- answer · `oklch(78.0% 0.105 230)` · links and short explanatory notices (`oklch(74.0% 0.130 145)` for positive echoes).
- marked · `oklch(81.0% 0.065 185)` text with `oklch(68.0% 0.095 185)` `2 px` left gutter bar · muted teal marking the user's own Draft and messages in a thread, plus search matches and choice keys.
- keys in motion · `oklch(82.0% 0.125 185)` · font color only (never a background pill) for `INS` mode and active keyboard hints.
- choice value · `oklch(66.0% 0.120 135)` · the value attached to a choice, not its name or key.
- point · `oklch(78.0% 0.012 95)` · where the user's attention is in the editor, and marks that guide the eye through text.
- current answer · `oklch(35.5% 0.022 150)` · the prompt row that Enter would take.
- seam · `oklch(32.0% 0.018 150)` · a crisp `1 px` square boundary (`oklch(27.8% 0.016 150)` for soft internal dividers).
- command strip · reading surface lifted slightly (`oklch(25.4% 0.019 150)`) · the area that takes a question, choice, or stuck draft.
- behind the sheet · black at `35%` opacity · the screen is still there but the phone menu is taking the tap.

## token type
- editor text · Rho Font (monospace), `15 px`, weight `400` · one crisp grid from the work to the prompt and top ears.
- reading rhythm · `1.54` times the editor text size (`Balanced`) · comfortable breathing room for reading long threads.
- emphasis · weight `650–700` · make the page title, unread name, `INS` mode, or menu title findable without making it larger.
- private aside · italic · the user's own remark rather than a turn in the exchange.
- thumb icon · `18 px` · recognize the action before reading its word.
- thumb label · `11 px` · confirm which action the icon will take.

## token space
- corner radius · `0 px` · sharp square corners on every bar, border, notch mask, code block, table, and callout.
- top line · `32–36 px` · three-part notch-native band: borderless Left Ear (`1fr`), square black center notch mask (`~210 px`), and framed Right Ear global status bar (`1fr`).
- sheet measure · `128 ch` centered · gives a calm side margin on a 13"–14" display while keeping Draft, prose, gutter bars, code blocks, diffs, tables, and images on one straight left vertical edge.
- prose measure · `80 ch` · wraps conversational prose, quotes, and private replies inside the `128 ch` sheet while letting code blocks, diffs, markdown tables, push cards, and images flow to the full `128 ch` sheet width.
- same band gap · `12 px` · separate items inside each top ear.
- reading inset · `24–28 px` · calm side breathing room from the window edge.
- seam width · `1 px` · crisp square boundary for the Right Ear global status bar and soft internal dividers.
- command strip breath · `2 px` above and below · keep prompt and menu close to the text rhythm.
- stuck draft shadow · `4 px` down, `12 px` blur, black at `35%` · separate the draft from the history below its seam without dimming the draft.
- draft gutter stroke · `2 px` teal left gutter bar beside every line of the thread draft and user messages · keeps the left edge fixed while distinguishing user voice from agent voice.
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
decided: 2026-11-26 · notch-native 3-part top row: borderless Left Ear owns page/card identity and page markers (no model name, no waits·age on Thread); center paints a square `#000` notch mask; full-width square-bordered Right Ear is the global status bar (echo, host exceptions, waiting lamp, quiet NOR / font-color-only teal INS with no pill)
purpose: separate what page/card is open from what rho is saying globally, using the two ears flanking the laptop's physical notch without spending vertical pixels below it.
looks: one `top line` (`32–36 px`, `0 px` radius) split into three columns:
- **Left Ear (page/card):** borderless on `reading surface`, visually part of the open buffer; holds the page title in `unread text` with `emphasis`, quiet project/context name (truncating title with ellipsis well before the center notch), and page-level markers (`pinned`, `approve push`, `working`, `stopped by you`, `stuck`, or active dealer focus on `desk.Desk`). Never shows the agent's model name on `chat.Thread` (model belongs in `agents.Transcript` and `:switch model`), and never repeats `waits · 4m` on `chat.Thread` (the newest unread message right below already displays its age).
- **Center (notch mask):** a solid `notch mask` (`#000000`) square rectangle framing the physical top-center camera notch so it reads with crisp `0 px` square corners.
- **Right Ear (global status bar):** fills the full right ear from the notch mask to the right window edge on `global status surface` with crisp `1 px` `seam` borders on its left and bottom edges (`0 px` radius); holds the `Echo line`, exception-only host notices (`<host> away` in `quiet`, `<host> needs you` in `lost connection`), the steady `attention` waiting lamp `●`, and the mode word (`NOR` in `quiet`, `INS` in bold `keys in motion` font color only — never a background pill).
states:
- ordinary: Left Ear shows the screen/thread title and project; Right Ear shows the waiting lamp (when other cards wait) and `NOR` (`INS` while typing or in live raw places).
- page exception: `pinned`, `approve push` (in `attention`), `working`, `stopped by you`, or `stuck` appear in the Left Ear beside the title/project.
- attention elsewhere: a steady `attention` dot `●` in the Right Ear before the mode; absent when nothing within active dealer focus waits.
- host state: silent when all hosts are online; an `away` host shows calmly in `quiet` in the Right Ear (turning `lost connection` red only when a host is in `needs you` state or when an `away` host is required by the screen currently in view).
- unpaired: a `quiet` notice in the Right Ear that verdicts stay on this device.
- echo: the `Echo line` appears inside the framed Right Ear without disturbing the Left Ear's page title or the mode word.
used on: `window.Screen`, including `desk.Desk`, `chat.Thread`, `agents.Transcript`, `browser.Page`, `notes.Notes`, and `usage.Chart`.
captures: [dealt clock thread on the laptop](design/laptop-card.png); [short heading on the phone](design/phone-menu.png); [transcript with its host, folder and activity](design/laptop-agent.png).

## part Echo line
decided: 2026-11-26 · lives inside the framed Right Ear global status bar; leaves the Left Ear page title untouched
purpose: answer the action the user just took without opening anything or shifting the page title.
looks: one line in `editor text` inside the framed Right Ear global status bar (`answer` / positive green for confirmations, `failed` for refusals), truncating cleanly if needed before the lamp and mode.
states:
- quiet: absent; the Right Ear stays calm with only exception notices, lamp, and mode.
- answer or refusal: the first line of the latest notice appears in the Right Ear, then clears after `echo life` or on the next prompt/key.
used on: `window.Screen`, including `chat.Thread` and `desk.Desk`.
captures: [done echo back on Desk](design/laptop-echo.png).

## part Row
purpose: let the user pick a place, command, or answer by name and see just enough context beside it.
looks: one row of `editor text`; name first in `reading text`, then the quieter context. On the laptop, `desk.Desk` puts `row air` between the subject and project address, or between a folded project's address and its stop count, with a fresh `chat.Flash` shown in `quiet` context when present. It shortens the subject only when it does not fit in the width left after the row's own detail; it never pads proportional text with spaces. The count uses the same face (adding any offline/connecting host note in `quiet`) and is not a selectable row. A narrow Desk row puts a folded project's stop count below its address. A prompt leaves a `reading inset` and gives the picked answer the `current answer` fill.
states:
- desk: a subject, a project address in `quiet`, and any fresh `chat.Flash` in `quiet`; a folded row of repeated stops uses the address in place of the subject and adds the count in `attention`.
- places (`window.Places`): place rows use `row air` between columns: subject/name, `attention` card words or the project's `quiet` latest words (or matched text snippet for full-text search), and `quiet` address. On a narrow window the subject cuts at a word, the address stays on the first line, and the words move to the second. Query matches use `marked`; the chosen row keeps its fill all the way across.
- commands (`window.Commands`): flat verb-noun command name in `reading text`, visually distinct parsed inline target/argument when typed, live target and current value in `quiet` / `choice value` (or `pick …` when ambiguous), and `quiet` key sequence (name and target only on a narrow window). Query matches use `marked`.
- empty: Desk reads `0 wait` (or `0 wait · lab away` in `quiet`) when no cards are due; `Places` and `Commands` show `no match` when no row fits the query.
used on: `desk.Desk`, `window.Places`, and `window.Commands`.
captures: [laptop Desk columns](design/laptop-desk.png), [phone Desk rows](design/phone-desk.png), and [matched palette row](design/laptop-prompt.png).

## part Message header
purpose: identify who wrote the message and when, before the user reads its words.
looks: the name in `marked` with `emphasis`, then a centered dot and `quiet` time, on a line above the message in `editor text`.
states:
- delivered: `you` or the project's address, and the time; an edited message adds `edited`.
- sending: the header reads `you · sending…` in `quiet` until the host has the message.
- private reply: `🔒 you ·` and time in `quiet` with `private aside`, like the words below it.
used on: `agents.Transcript`. In `chat.Thread` the voice is told by colour, see `Message`.
form: `you · 2m` or `clock@lab · 1m · edited` above the words; see `Message` for the separate thread form.

## part Message
decided: 2026-11-26 · agent is primary unmarked reading voice (bright unread, softer read); user is muted teal with 2px left gutter bar; right-aligned ages (amber unread, quiet older); 128ch centered sheet with 80ch prose wrap and full-sheet code/diffs/tables/images; no date separators, no history folding
purpose: let the user read an exchange and know who said what immediately, with nothing between messages but clean space.
looks: laid out inside the `sheet measure` (`128 ch` centered). Prose paragraphs, quotes, and private replies wrap at `prose measure` (`80 ch`); code blocks, diffs, markdown tables, push cards, and images expand across the full `128 ch` sheet on the same straight left edge (`0 px` corners). The project/agent's message is the primary unmarked reading voice (`unread text` when unread since the user's last reply, softer `reading text` once read) with no bar; the user's own message uses muted teal (`marked`) with a `2 px` teal bar in the left gutter beside every line. Each turn's relative age sits flush-right on its first line (`attention` amber on unread turns, `quiet` on older turns). No date separator rows and no collapsed history folds. A `quiet` name line appears above a message from a third voice (or agent short code when multiple agents speak live); `· edited` or `· taken back` sits in `quiet` at the end of the prose.
states:
- flash: a `chat.Flash` in `quiet`, its age in `quiet` flush-right on its first line; an agent's earlier fresh flashes fold to one `quiet` line, `+2 earlier`.
- sending: the user's words in `quiet` until the host has the message.
- private reply: `🔒 you ·` and time in `quiet` with `private aside`, like the words below it.
used on: `chat.Thread`.
form: [project and user message on the laptop](design/laptop-card.png) and [the open thread](design/laptop-reading.png).

## part Quote
purpose: let the user read the words another message is answering without mistaking them for new words.
looks: quoted Markdown keeps its `>` marker in the reading line. The words use `editor text` on the `reading surface`, without a separate quote panel.
states:
- plain: the body has no quote marker.
- quoted: `>` starts the quoted line; a block of several lines starts folded to its first line and `…`. The reply can follow in ordinary text.
used on: built host thread (`chat.Thread`) and transcript (`agents.Transcript`) when the writer includes quoted Markdown.
form:
```text
> Fix the clock test that fails one run in five.
A separate clock per test should do it.
```

## part Draft
decided: 2026-11-26 · row 1 of Thread directly below the top ears, 2px teal left gutter bar, zero `reply` placeholder text, 80ch prose wrap inside the 128ch sheet
purpose: let the user write the next message without leaving the buffer.
looks: the user's words in muted teal (`marked`), matching their sent messages, on row 1 at the top of the `128 ch` sheet (wrapping at `80 ch`), separated from the newest message below by a soft `1 px` horizontal hairline seam, with a `2 px` teal bar (`draft gutter stroke`) in the left gutter beside every draft line.
states:
- empty: in a `chat.Thread`, no `reply` placeholder text — just the `2 px` teal left gutter bar and the editor cursor on a clean empty line (the user already knows row 1 is the reply draft). In `chat.Write an idea`, the header lines (`to:`, `in:`, `on:`) establish the empty idea buffer.
- stuck in a thread: the same text and gutter origin at the top edge, on the `command strip` surface, with a soft `seam` and `stuck draft shadow` below; `↑ new` appears right-aligned in `attention` when unseen messages arrive while scrolled, without moving the draft or history.
- writing: the cursor sits in the draft and the Right Ear mode word says `INS` in teal font color (no pill); on return to `NOR`, the cursor remains in the draft.
- addressing an idea: `in:` (`one` or `new`) and `on:` (runtime) sit under `to:` above the words. The addressed project’s mode and runtime appear in `quiet` until the user edits or chooses them. Changing `to:` updates untouched quiet values but keeps explicit choices. Enter on `in:` or `on:` opens the choices.
- after send: in a thread, the words become the newest message right below the draft, marked while on their way, with the draft empty and nothing scrolling. For an idea, the draft clears immediately, the echo says where it went in the Right Ear, and the screen before returns.
used on: `chat.Thread` and the draft of `chat.Write an idea`.
form: [empty reply](design/laptop-card.png), [empty idea with the project runtime](design/laptop-idea-empty.png), [idea with an explicit runtime on phone](design/phone-idea-runtime.png), and a stuck reply with new message on [laptop](design/laptop-reading-notice.png) and [phone](design/phone-reading-notice.png).

## part Menu
purpose: offer a few keyed choices without taking the user away from the thing they are on.
looks: on a laptop an `emphasis` title and compact `menu column` grid sit in the `command strip` over the bottom of the work; `marked` keys lead each column. Available values in parentheses use `choice value`. On a phone `thumb target` rows start with their action, end with the key in `keys in motion`, and have `behind the sheet` over the work.
states:
- closed: the work fills its space, and no menu is visible.
- open: title, actions, keys, and any values; on the phone the sheet has `close` at the top and a full-width `cancel` at the bottom edge.
- sub-menu: it takes the same place; the phone sheet adds `‹ back`.
- empty: no sheet or grid; the `Echo line` says `nothing here` with the menu name.
used on: `window.Screen` wherever the `space` leader `window.Menu` opens (`f` places and `u` undo everywhere in `NOR`; `s` snooze reminder on `notes.Note`; card verdicts — or `a` approve / `x` deny / `s` snooze for a pending push — added on `chat.Thread` and `agents.Transcript` of a waiting card).
form: [laptop verdict grid](design/laptop-menu.png) and [phone verdict sheet](design/phone-menu.png).

## part Prompt
purpose: ask for a line of text and offer answers without a dialog.
looks: `editor text` in the `command strip`, question and input first with a `reading inset`, then the candidate rows down the window. The input wraps below a long question on a narrow screen.
states:
- blank query: question and any initial answers.
- typing: text narrows the list; moving the selection keeps the `current answer` among `prompt answers`. `Places` and `Commands` show the selected position and total in `quiet` at the right end of their prompt line.
- no match: `no match` below the input when a choice is required and none fits.
- closed: the strip goes away and the keys return.
used on: `window.Places`, `window.Commands`, and other questions over `window.Screen`, as in `hosts.Attach a host`.
form: [filtered laptop palette](design/laptop-prompt.png) and [blank phone palette](design/phone-prompt.png).

## part Phone bar
purpose: give the thumb the actions that the current place can use.
looks: one `thumb target` band with a `seam` above and equally wide buttons; each `thumb icon` sits over its `thumb label`, both in `quiet`.
states:
- desk: menu, open, and more; open deals the top card and a tap on a row opens that row.
- card open: places, menu, done, snooze, and more; while writing, undo and send take their place.
- workset transcript for an open card: places, done, snooze, more, and menu. Verdicts land on the card.
- reading elsewhere: places, desk, and menu; while writing, undo and send join.
- writing an idea: cancel, undo, and send.
- prompt: only cancel and enter.
- menu sheet: the sheet takes taps in front of the work, and its cancel target covers the bar.
used on: `window.Screen` at `narrow window` width, including `desk.Desk`, `chat.Thread`, `agents.Transcript`, `window.Places`, and `window.Commands`.
form: [Desk](design/phone-desk.png), [card with send](design/phone-card.png), [transcript without send](design/phone-agent.png), and [palette](design/phone-prompt.png).

## part Card's words
purpose: say how long something has waited for the user, or why the work stopped.
looks: `waits`, or a stop's word, a centered dot, and the age in `attention`, beside the heading in the `Status line` and on a thread row in `window.Places`. On a phone the heading shortens before these words.
states:
- waiting or stopped: the word and the age, not just the color.
- snoozed thread: it leaves the Desk waiting rows until it can be dealt again.
- nothing waiting: no card words; ordinary row information remains.
- echo: the words give way on the top line until the answer is gone.
used on: `chat.Thread` when open as a card, and `window.Places`; not on `desk.Desk`, which is a glance.
form: [ask on the phone's top line](design/phone-card.png).

## part Allowance row
purpose: say how much an account has left and when it resets without making the user read the picture.
looks: one line of `editor text` per account above the picture, with `row air` before the share left and a centered dot before the reset time. The row uses its line's color, and `★` marks the active account the host is using (for GPT or Claude).
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

## rule the echo lives in the right ear
when the `Echo line` answers, then it appears inside the framed Right Ear global status bar beside the lamp and mode without displacing the Left Ear's page title or page markers.

## rule a prompt takes the phone bar
when a `Prompt` opens on a phone, then the `Phone bar` offers its cancel and enter instead of the work's actions.

## rule the menu leaves the work where it is
when a `Menu` opens, then the `Row` or `Draft` behind it keeps its point and the choices take the keys.
