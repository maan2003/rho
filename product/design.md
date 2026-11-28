# design

The built window puts the work in one editor with sharp `0 px` square corners throughout (no rounded pills or cards), tuned primarily for a fullscreen 13" laptop display (`MacBook Air M2`, Asahi Linux + Niri, with a physical top-center camera notch). There is no frame-owned left header bar: each screen's own content fills 100% of the window from `y = 0` and reserves space for only two global top overlays — a **square black center notch mask** and the borderless **Top-Right Global Status Bar**, which has no field of its own and lies on the screen's own surface, so the top row reads as one seamless band split only by the notch. Values are for the bundled dark appearance, Rho OKSolar P3; sizes are in logical pixels. The [laptop and phone renders](design/README.md) show the states and how to reproduce them.

## token color
- reading surface · `oklch(23.0% 0.018 150)` · the continuous field the user reads and writes on.
- notch mask · `#000000` · solid square black rectangle framing the physical top-center notch so it reads as crisp square geometry.
- reading text · `oklch(70.9% 0.012 95)` · every agent turn and body text, read or unread, in one calm OKSolar voice (no brightness split between unread and read).
- title · `oklch(78.0% 0.012 95)` at the text's own weight · a page heading's title, one step brighter than the text.
- quiet · `oklch(56.0% 0.012 110)` · context that can wait: every turn age, settled things, Desk's `away` tag, host tags, calm `away` host notes in the Top-Right Global Status Bar, `NOR` mode, and hints.
- attention · `oklch(79.5% 0.145 78)` · something to act on: pending push approval (`approve push` on Desk and the thread title row) or the waiting lamp. Never a turn age.
- lost connection · `oklch(63.1% 0.160 21.6)` · a host the user cannot reach (`needs you`, or an `away` host required by the open screen).
- failed · `oklch(75.0% 0.145 25)` · a step that did not finish successfully.
- answer · `oklch(78.0% 0.105 230)` · links and short explanatory notices (`oklch(74.0% 0.130 145)` for positive echoes).
- marked · `oklch(72.0% 0.072 185)` text with `oklch(60.0% 0.085 185)` `2 px` left gutter bar · muted teal marking the user's own Draft and messages in a thread, plus prompt query matches and choice keys.
- search match · opaque `oklch(35.0% 0.085 85)` background highlight under the text's own color (at least `4:1` for both `reading text` and `marked`), also the selection, since a `/` match is selected · makes `/` and `?` matches stand out clearly inside both agent prose and muted-teal user messages.
- keys in motion · `oklch(82.0% 0.125 185)` · font color only (never a background pill) for `INS` mode and active keyboard hints.
- choice value · `oklch(66.0% 0.120 135)` · the value attached to a choice, not its name or key.
- point · `oklch(78.0% 0.012 95)` · where the user's attention is in the editor, and marks that guide the eye through text.
- current answer · `oklch(35.5% 0.022 150)` · the prompt row that Enter would take.
- seam · `oklch(32.0% 0.018 150)` · a crisp `1 px` square boundary (`oklch(27.8% 0.016 150)` for soft internal dividers).
- command strip · reading surface lifted slightly (`oklch(25.4% 0.019 150)`) · the area that takes a question or choice, and `chat.Thread`'s canopy while its draft is stuck.
- behind the sheet · black at `35%` opacity · the screen is still there but the phone menu is taking the tap.
- behind the prompt · black at `35%` opacity · the laptop screen and the status right of the notch are still there while the prompt panel takes the keys.

## token type
- editor text · Rho Font (monospace), `15 px`, weight `400` · one crisp grid from the work to the prompt and top ears.
- reading rhythm · `1.54` times the editor text size (`Balanced`) · comfortable breathing room for reading long threads.
- emphasis · weight `650–700` · make an unread name, `INS` mode, or menu title findable without making it larger. Page heading titles stay at weight `400`: `title` color alone sets them apart.
- private aside · italic · the user's own remark rather than a turn in the exchange.
- thumb icon · `18 px` · recognize the action before reading its word.
- thumb label · `11 px` · confirm which action the icon will take.

## token space
- corner radius · `0 px` · sharp square corners on every bar, border, notch mask, code block, table, and callout.
- top line · `32–34 px` · global top overlays only: center square black notch mask (`~210 px`, toggleable via `toggle notch mask`) and the borderless Top-Right Global Status Bar (right of notch to window edge, on the screen's own surface). Screen content owns `100%` of the window from `y = 0`, reserving space for those overlays (raw guest surfaces start below the `32 px` top row with a quiet rho title row at top-left).
- top gap · `14 px` · air between the `top line` and row 1 of a screen's sheet (laptop `sheet::page` screens and `desk.Desk`; the phone layout has none).
- heading edge · a screen's top-left heading starts on its sheet's left edge (where the sheet's words start), clamped to `reading inset` on narrow windows, and is cut a `same band gap` before the notch mask.
- sheet measure · `128 ch` centered · gives a calm side margin on a 13"–14" display while keeping Draft, prose, gutter bars, code blocks, diffs, tables, and images on one straight left vertical edge.
- prose measure · `80 ch` · wraps conversational prose, quotes, and private replies inside the `128 ch` sheet while letting code blocks, diffs, markdown tables, push cards, and images flow to the full `128 ch` sheet width.
- same band gap · `12 px` · separate items inside each top ear.
- reading inset · `24–28 px` · calm side breathing room from the window edge.
- seam width · `1 px` · soft internal dividers and the prompt panel's faint edge.
- command strip breath · `2 px` above and below · keep prompt and menu close to the text rhythm.
- canopy band · one `reading rhythm` row below a stuck draft's last line · holds `↑ new` and ends the canopy on a row boundary, so the canopy's lower edge never cuts through a line of the history below it.
- draft gutter stroke · `2 px` teal left gutter bar beside every line of the thread draft and user messages, with `10 px` of air before the first letter (a `12 px` sheet gutter) · keeps the left edge fixed while distinguishing user voice from agent voice.
- thumb target · `56 px` high · an action the thumb can hit at the bottom edge or in a menu sheet.
- narrow window · up to `600 px` wide · make room for thumb controls rather than using a desktop overlay.
- row air · two text spaces · leave space between a row name and its words.
- prompt panel · `104 ch` floating square panel with its top at `y = 92 px` (`58 px` below the `top line`), on a lifted `oklch(27.6% 0.020 150)` surface with a faint `1 px` `oklch(30.5% 0.016 150)` edge, a step lighter `oklch(33.5% 0.018 150)` on top, and no shadow, over the screen dimmed by `behind the prompt` · brings `Places`, `Commands`, and `:snooze` into natural eye line without jumping to the bottom edge.
- prompt answers · up to eight visible rows · keep the answer being considered in sight without making a second screen.

## token time
- echo life · `4 s` · long enough to see the result of the last action in the Top-Right Global Status Bar (temporarily hiding quiet notices; full history in `show messages`).
- age beat · `1 min` · refresh elapsed ages on the thread and its list without a ticking clock on every row.
- card age step · `now` during the first minute, nearest minute thereafter under an hour, tenth of an hour under a day, tenth of a day after that · change the words when the displayed age changes.
- while needed · event-bound, no fixed duration · the signal or open choice stays while it is true or still takes the next action.
- cut · immediate, no transition · hand the keys or the line to the newly active thing.
- thumb settle · `180 ms` · finish the phone page move or return it to where it was.

## part Status line
decided: 2026-11-28 · no frame-owned left ear; active screen content fills 100% from y=0 and reserves space for only two global top overlays — center square `#000` notch mask (~210px × 32px) and the borderless Top-Right Global Status Bar (echo, host exceptions, waiting lamp, quiet NOR / font-color-only teal INS with no pill) on the screen's own surface, dimmed with the screen when a prompt opens
purpose: keep global rho status (echo, degraded hosts, waiting lamp, mode) in the free top-right space next to the laptop's physical notch without adding a full-width chrome bar or stealing vertical pixels from the screen's own content.
looks: the active screen fills 100% of the window from `y = 0` and owns the top-left zone beside the notch for its own content, starting on its sheet's left edge (`heading edge`): page title in `title` at weight `400`, project/count, page markers — omitting model name and redundant `waits · 4m` on `chat.Thread`; `agents.Transcript` puts its model name there). Over the top edge (`32–34 px`, `0 px` radius), the frame paints only:
- **Center (notch mask):** a solid `notch mask` (`#000000`) square rectangle (`~210 px`) framing the physical top-center camera notch with crisp `0 px` square corners. Can be toggled off manually via `toggle notch mask` in `window.Commands` (when off, the black rectangle disappears cleanly).
- **Top-Right Global Status Bar:** fills from the right edge of the notch mask to the right window edge with no field, border, or seam of its own: its words lie on whatever the screen paints there (`reading surface`, or `chat.Thread`'s canopy while its draft is stuck), so the top row reads as one seamless band split only by the notch. It holds the `Echo line` (which temporarily hides quiet notices during its `echo life` so it has full room), active dealer focus (`focus tide` / `quiet 32m` in `quiet`), exception-only host notices (`<host> away` in `quiet`, `<host> needs you` in `lost connection`), the steady `attention` waiting lamp `●`, and the mode word (`NOR` in `quiet`, `INS` in bold `keys in motion` font color only — never a background pill).
states:
- ordinary: screen content owns the full canvas (including top-left title row beside the notch; raw guest surfaces like Terminal/Browser/Desktop start below the `32 px` top row with a quiet rho title row at top-left); the Top-Right Global Status Bar shows active dealer focus (when set), the waiting lamp (when other cards wait), and `NOR` (`INS` while typing or in live raw places).
- attention elsewhere: a steady `attention` dot `●` in the Top-Right Global Status Bar before the mode; absent on `desk.Desk` or when nothing within active dealer focus waits (away-host cards stay off the lamp and chime).
- host state: silent when all hosts are online; an `away` host shows calmly in `quiet` in the Top-Right Global Status Bar (turning `lost connection` red only when a host is in `needs you` state or when an `away` host is required by the screen currently in view).
- unpaired: a `quiet` notice in the Top-Right Global Status Bar that verdicts stay on this device.
- echo: the `Echo line` appears in the Top-Right Global Status Bar, temporarily hiding quiet notices (`gpu-box away`, unpaired note) during its `echo life` without disturbing the screen's content, lamp, or mode word; recent echoes remain readable via `show messages` in `window.Commands`.
- prompt open: the `behind the prompt` dim covers the entire screen content layer and the Top-Right Global Status Bar alike (the screen's top-left title row and the status right of the notch dim together), leaving only the `104 ch` prompt panel and the center notch mask (if enabled) bright.
used on: `window.Screen`, including `desk.Desk`, `chat.Thread`, `agents.Transcript`, `browser.Page`, `notes.Notes`, and `usage.Chart`.
captures: [dealt clock thread on the laptop](design/laptop-card.png); [short heading on the phone](design/phone-menu.png); [transcript with its host, folder and activity](design/laptop-agent.png).

## part Echo line
decided: 2026-11-26 · lives in the borderless Top-Right Global Status Bar, temporarily hiding quiet notices so it has full room; full history via `show messages`; leaves screen content untouched
purpose: answer the action the user just took without opening anything or shifting screen content.
looks: one line in `editor text` in the Top-Right Global Status Bar (`answer` / positive green for confirmations, `failed` for refusals), temporarily hiding quiet notices (`gpu-box away`) so it fits cleanly before the lamp and mode.
states:
- quiet: absent; the Top-Right Global Status Bar stays calm with focus, exception notices, lamp, and mode.
- answer or refusal: the first line of the latest notice appears in the Top-Right Global Status Bar, then clears after `echo life` or on the next prompt/key; recent echoes stay available via `show messages`.
used on: `window.Screen`, including `chat.Thread` and `desk.Desk`.
captures: [done echo back on Desk](design/laptop-echo.png).

## part Row
decided: 2026-11-27 · Desk is a pure view-only 1-line tabular glance inside a 92ch table (no cursor, no row selection; row 1 = Card 1, count in Desk's top-left title row, no subheader; columns: subject · exception tag · project · quiet right age; host away stays in Top-Right Global Status Bar only); Places and Commands live inside the upper-third 104ch square prompt panel with keys shown quietly flush-right
purpose: let the user glance at waiting work on Desk or pick a place, command, or snooze time cleanly in tabular alignment.
looks: one row of `editor text` (`0 px` corners).
- on `desk.Desk`: pure view-only tabular glance laid out inside a `92 ch` centered table starting at row 1 after the `top gap` (`desk · N wait` sits in Desk's own top-left title row beside the notch, on the table's left edge; `away` hosts stay in the Top-Right Global Status Bar — never repeated inside the Desk body). Each waiting card is one crisp tabular row: subject (truncated only if needed), optional exception tag (`approve push` in `attention`, a stop such as `failed` or `3 stopped` in `failed`, `away` in `quiet`), right-aligned `quiet` project address, and right-aligned `quiet` age (`3.0h`). **No text cursor and no row-selection highlight** — the user glances and presses `space-j`, `i`, `space f`, `tab`, or `:` (clicking/tapping a row opens it directly). `and N more · …` sits quietly below the visible rows.
- on `window.Places`: inside the upper-third `104 ch` square prompt panel; columns show clean subject/name (never raw temp paths for visited transcripts), `attention` card words or `quiet` latest words (or matched text snippet for full-text search), `quiet` project/kind, and flush-right key sequence if the place has a shortcut. Query matches use `marked`; the highlighted prompt row uses `current answer` fill with a `2 px` left `keys in motion` bar.
- on `window.Commands`: inside the upper-third `104 ch` square prompt panel; lists **every** action in rho (including `deal next`, `close and deal`, `done`, `mute`, `unmute`, `snooze thread`, `snooze note`, `unsnooze thread`, `unsnooze note`, `approve push`, `deny push`, `undo`, `open places`, `open desk`, `close screen`, `show messages`, `toggle notch mask`). Each row shows flat verb-noun command name in `reading text`, visually distinct parsed inline target/argument (or live resolved snooze timestamp preview), live target/current value in `quiet` / `choice value`, and its key sequence quietly flush-right. Query matches use `marked`.
states:
- desk: up to seven 1-line tabular rows in deal order, then `and N more · …` for overflow; counts from local cache while hosts connect (`connecting` in Top-Right Global Status Bar — never flashing a false `0 wait`).
- places (`window.Places`): MRU rows or search/full-text matches with highlighted matches.
- commands (`window.Commands`): all rho commands (recents first when blank) with inline arguments, live target/preview, and flush-right shortcut keys.
- empty: Desk reads `desk · 0 wait` in its top-left title row once all attached hosts answer; `Places` and `Commands` show `no match` when no row fits the query.
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
decided: 2026-11-27 · agent is primary unmarked reading voice in one calm `reading text` (no unread/read split); user is muted teal with 2px left gutter bar and 10px of air; right-aligned ages all `quiet`; 128ch centered sheet with 80ch prose wrap and full-sheet code/diffs/tables/images; no date separators, no history folding
purpose: let the user read an exchange and know who said what immediately, with nothing between messages but clean space.
looks: laid out inside the `sheet measure` (`128 ch` centered). Prose paragraphs, quotes, and private replies wrap at `prose measure` (`80 ch`); code blocks, diffs, markdown tables, push cards, and images expand across the full `128 ch` sheet on the same straight left edge (`0 px` corners). The project/agent's message is the primary unmarked reading voice, in `reading text` whether read or not, with no bar; the user's own message uses muted teal (`marked`) with a `2 px` teal bar in the left gutter beside every line and `10 px` of air before the words. Each turn's relative age sits flush-right on its first line in `quiet`; the card, not the age, says that the thread wants the user. No date separator rows and no collapsed history folds. A `quiet` name line appears above a message from a third voice (or agent short code when multiple agents speak live); `· edited` or `· taken back` sits in `quiet` at the end of the prose.
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
decided: 2026-11-28 · row 1 of Thread directly below the top ears, 2px teal left gutter bar, zero `reply` placeholder text, 80ch prose wrap inside the 128ch sheet; no divider line below it; stuck, it lifts the whole top zone into one borderless canopy
purpose: let the user write the next message without leaving the buffer.
looks: the user's words in muted teal (`marked`), matching their sent messages, on row 1 at the top of the `128 ch` sheet (wrapping at `80 ch`), separated from the newest message below by space alone (a blank line, no divider line), with a `2 px` teal bar (`draft gutter stroke`) in the left gutter beside every draft line.
states:
- empty: in a `chat.Thread`, no `reply` placeholder text — just the `2 px` teal left gutter bar and the editor cursor on a clean empty line (the user already knows row 1 is the reply draft). In `chat.Write an idea`, the header lines (`to:`, `in:`, `on:`) establish the empty idea buffer.
- stuck in a thread (the Unified Tonal Canopy): the whole top zone, from `y = 0` down to one `canopy band` below the draft's last line, becomes one borderless canopy in `command strip` across the full window width. It takes in the thread's heading, the `top gap`, the draft, and the Top-Right Global Status Bar on the other side of the notch. Its tone alone sets it apart from the history below: no seam, no hairline, and no shadow. The draft's text and gutter bar keep the exact `x` and `y` they had inline (`0 px` shift), and no pixel of the history shows above the canopy's lower edge. The only words added are `↑ new`, right-aligned in `attention` in the band, when unseen messages arrive while scrolled; it moves neither the draft nor the history.
- writing: the cursor sits in the draft and the Top-Right Global Status Bar's mode word says `INS` in teal font color (no pill); on return to `NOR`, the cursor remains in the draft.
- addressing an idea: `in:` (`one` or `new`) and `on:` (runtime) sit under `to:` above the words. The addressed project’s mode and runtime appear in `quiet` until the user edits or chooses them. Changing `to:` updates untouched quiet values but keeps explicit choices. Enter on `in:` or `on:` opens the choices.
- after send: in a thread, the words become the newest message right below the draft, marked while on their way, with the draft empty and nothing scrolling. For an idea, the draft clears immediately, the echo says where it went in the Top-Right Global Status Bar, and the screen before returns.
used on: `chat.Thread` and the draft of `chat.Write an idea`.
form: [empty reply](design/laptop-card.png), [empty idea with the project runtime](design/laptop-idea-empty.png), [idea with an explicit runtime on phone](design/phone-idea-runtime.png), and a stuck reply with new message on [laptop](design/laptop-reading-notice.png) and [phone](design/phone-reading-notice.png).

## part Prompt
decided: 2026-11-28 · floating 104ch square panel with its top at y = 92px, lifted above the screen by its 27.6% surface and a faint 1px edge (30.5%, 33.5% on top) with no shadow, over a 35% black dim of the screen, its heading AND the status right of the notch, so only the panel and the notch mask stay bright; Menu removed completely — space is a silent prefix key with no popup/hint, and space s prefills :snooze thread or :snooze note in this same Prompt
purpose: ask for a line of text, pick a place, run any command, or pick a snooze duration in one consistent overlay.
looks: on desktop, a floating `prompt panel` (`104 ch` wide, `0 px` corners, top at `y = 92 px`, lifted surface, faint edge a step lighter on top, no shadow). Opening a prompt dims the screen body, its top-left heading, and the Top-Right Global Status Bar uniformly (`behind the prompt`), leaving only the prompt panel and the center notch mask at full brightness. Question/prefix and input line sit at the top separated by a soft `1 px` divider, with selected position/total in `quiet` at the right end, followed by candidate rows below.
states:
- blank query: question/prefix and initial answers (MRU places in `Places`, recent + all commands in `Commands`, or quick duration presets with live resolved timestamps when prefilled with `:snooze thread ` / `:snooze note `).
- typing: text narrows the list (or previews custom snooze times live); moving the selection keeps the `current answer` among `prompt answers`.
- no match: `no match` below the input when a choice is required and none fits.
- closed: the panel disappears immediately and keys return to the screen underneath.
used on: `window.Places`, `window.Commands` (including `:snooze thread` / `:snooze note`), and other questions over `window.Screen`, as in `hosts.Attach a host`.
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
purpose: say how long a thread has waited for the user, or why the work stopped, when scanning rows in `window.Places`.
looks: `waits`, or a stop's word, a centered dot, and the age in `attention`, on a thread row in `window.Places`. (Omitted from an open `chat.Thread`'s title row, where the newest unread message already shows its age.)
states:
- waiting or stopped: the word and the age, not just the color.
- snoozed thread: shows `snoozed` in `window.Places` and leaves `desk.Desk` until due.
- nothing waiting: no card words; ordinary row information remains.
used on: `window.Places` (and phone narrow headings); not on an open desktop `chat.Thread` title row.
form: [ask on the phone's top line](design/phone-card.png).

## part Allowance row
purpose: say how much an account has left and when it resets without making the user read the picture.
looks: one line of `editor text` per account above the picture, with `row air` before the share left and a centered dot before the reset time. The row uses its line's color, and `★` (adding host name like `★ lab` when more than one host is attached) marks the active account each host is using.
states:
- reported: `★ lab · gpt/work  66% left · resets in 5.0d`.
- no reset time: `gpt/home  18% left · reset time not reported`.
- reset time passed: `gpt/home  18% left · last report before reset`.
- no allowance from this account yet: `gpt/home  allowance not reported`.
used on: `usage.Chart`.
form:
```text
★ lab · gpt/work  66% left · resets in 5.0d
gpt/home  18% left · resets in 2.1d
```

## motion An answer shows in the status bar
means: the last action has an answer; the Echo line shows in the Top-Right Global Status Bar (temporarily hiding quiet notices) without moving screen content, then clears automatically.
lasts: `echo life`; a new answer replaces the old one and a prompt takes it away.

## motion The lamp switches on or off
means: something important enough wants the user elsewhere, not the card already in view; the dot is steady, not a pulse.
lasts: `while needed`; at a higher crossing the chime sounds once, not on each refresh.

## motion The ages catch up
means: relative ages change in place as the work waits, without moving the user's point.
lasts: `age beat` on thread summaries; `card age step` for the deal's words.

## motion A prompt takes the keys
means: the upper-third square prompt panel is present while it takes a choice; it covers the dimmed work instead of reflowing it.
lasts: `cut` on open, replacement, or close; otherwise `while needed`.

## motion A phone page settles
means: a flick either turns the page or gives it back; the brief slide shows which happened.
lasts: `thumb settle`.

## rule the echo lives in the top-right status bar
when the `Echo line` answers, then it appears in the Top-Right Global Status Bar (temporarily hiding quiet notices during `echo life`) beside the lamp and mode without displacing the screen's title row.

## rule a prompt takes the phone bar
when a `Prompt` opens on a phone, then the `Phone bar` offers its cancel and enter instead of the work's actions.
