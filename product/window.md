# window

The one window that every screen sits in: one screen at a time, the line at the top that says where the user is, and the ways to reach any place, any command, and the next waiting card by keys or by thumb.

## bet One window becomes the operating system
instead of: rho beside a terminal, a browser, and other apps on the laptop.
serves: Reach anything.
because: everything the user does should be a place with the same keys and the same way to switch; terminals, desktops, and the browser come inside, and what the user uses most becomes a place of its own.
costs: what has no place yet is reached through a terminal, the browser, or an agent.

## concept Screen
purpose: show one thing at a time, so the user is never looking at two things that compete for them.
principle: the user opens a `chat.Thread` from the desk; `tab` returns to the desk, and `space` then `f` + `enter` flips straight back to the place they were in before.
sees: the screen's text or guest surface, filling the window under the `Status line`; nothing around it.
does:
- places `space` then `f`: open `Places` (most recently visited places first; `enter` with nothing typed flips to the previous place)
- commands `:` (or `ctrl-shift-;` from a raw place or while typing): open `Commands`
- deal `space-j` (or `ctrl-j` in `NOR`): deal the next waiting card, as `deal.Card.deal`
- close `q`, or `ctrl-shift-backspace` while typing: close this screen and return to the previous recent place (or `desk.Desk`)
- close and deal `space-shift-j` (or `ctrl-shift-j` in `NOR`): close this screen and deal the next card
- desk `tab`: go to `desk.Desk`; on the desk, return to the previous place
- leader `space` in `NOR`: open the `Menu` (`f` places, `u` undo when undoable, plus card verdicts when the screen is a waiting card, or `s` snooze on a `notes.Note`)

Navigation has no linear back/forward history stack: the user moves purely by `Places` MRU (`space` then `f`), `desk.Desk` (`tab`), and `deal.Card.deal` (`space-j`). Each open screen keeps its own scroll position and cursor when revisited. While a host is still connecting after a restart, a host-backed screen shows its locally cached view or a connecting placeholder instead of being discarded. Returning after more than ten minutes opens the desk; see `desk.a cold return opens the desk`.

At the OS keyboard layer (`keyd`), logical gestures map to unused function keys so they work globally without colliding with typing: `space-j` (hold `space` + `j`) → `f20`, `space-shift-j` (hold `space` + `shift-j`) → `f16`, and `physical-escape` (physical `escape` key, forwarded as literal `escape` to a guest) → `f9`, while tapping `space` sends `space` (the `NOR` leader) and tapping `capslock` sends `escape` (rho `NOR`). Note on the user's keyboard setup: `keyd` uses a tuned `lettermod(movement, space, 95, 150)` home-layer hold that already handles typing rollover cleanly in daily use, so hold-`space` chords are intentional and reliable even in `INS`; reviewers should treat OS-level `keyd` rollover as solved. Code keeps one explicit constant table mapping these logical gestures to their key symbols so every binding stays readable. `space-j` and `space-shift-j` work globally, including from raw live places; other `space+` OS keys stay unbound.

Every editor screen is text in an editor. Helix's selections, motions, search, and modes work on every editor screen, so the user moves through a list of threads the same way they move through a thread or a chart's numbers. `/` and `?` ask `search:`, `n` and `N` go on, a search wraps round the end and says so, and `G` goes to the end. A search selects the whole match, so `c` changes it and `d` deletes it.

Only screens that represent a waiting item (`chat.Thread` — including one with a pending push/fetch approval — a `Workset` transcript inspecting a waiting card, or a snoozed `notes.Note` reminder when dealt) offer card verdicts in the `space` `Menu`; non-card screens (`desk.Desk`, `browser.Page`, `usage.Chart`, `agents.Terminal`, `agents.Shell`, `agents.Desktop`) only offer `f` places and `u` undo (and a standalone `notes.Note` adds `s` snooze for reminders).
rejected:
- a linear back/forward history stack (`ctrl-k` / `ctrl-j` / `f21`) · 2026-11-26 · dealing through cards piled up dead history and broke 1-gesture return; pure MRU `Places` (`space f`), `Desk` (`tab`), and `Deal` (`space-j`) are simpler and never trap the user
- a separate pre-deal return point or excluding dealt cards from `Places` MRU · 2026-11-26 · pure MRU is simpler and predictable; after clearing a batch of cards, the user jumps back to their place by name in `Places`

## rule raw places share one dual-escape model
when the user is in a raw-input place (`browser.Page`, `agents.Terminal`, `agents.Desktop`, or typing in `agents.Shell`), then live input (`INS`) sends ordinary keys (including `space`) straight to the guest without leaking reserved window chords; pressing `escape` always drops the screen into rho `NOR`, where `space` (`f` places, `u` undo), `tab`, `q`, `:`, and `ctrl-j` work identically across every place (`i` or clicking the guest returns to `INS`); pressing `physical-escape` forwards a literal `escape` to the guest; `ctrl-shift-;` opens `Commands`, `ctrl-alt-m` opens an idea draft, and `space-j` / `space-shift-j` deal directly in one step.
rejected:
- separate per-place exit chords (`ctrl-shift-f9/f10/f11` in Browser, `ctrl-shift-n` in Terminal, `space` hijack in Desktop) · 2026-11-26 · broke muscle memory and typing spaces into remote apps

## concept Edge-stuck excerpt
purpose: let the user keep working in one part of the text while reading the rest of it.
principle: the user scrolls down to read an old `chat.Message` while typing a `chat.Draft`; the draft remains editable at the top edge, with history below it.
sees: a designated excerpt as ordinary text while any of it is in the window; when it has left the window past one edge, the excerpt sits over that edge, the top for a `chat.Draft`. It can take up to half the window; if taller, its text scrolls inside that space.
does:
- scroll the rest `ctrl-u`, `ctrl-d`, or by thumb outside the excerpt: move the view, not the point
- move and search: use the same motions as one buffer; `j` from the last line of a top excerpt moves into the text below it, `G` goes to the end

## concept Status line
decided: 2026-11-26 · three-part notch-native top strip (borderless Left Ear = open page/card identity & page markers; center = square black notch mask; full-width square-framed Right Ear = global status bar with echo, degraded host notices, waiting lamp, and quiet NOR / font-color-only teal INS); no model name or waits·age on open Thread
purpose: tell the user, at a glance, what page is open on the left and what rho is saying globally on the right, using the two ears flanking the laptop's physical top-center notch without spending vertical height below it.
principle: the user opens a thread that asks for them; the borderless Left Ear names the thread and project (`Fix the flaky retry clock test  tide`), the center frames the physical M2 notch in square `#000`, and the square-bordered Right Ear stays calm with `●  NOR`; they say done, and for a few seconds the Right Ear reads `done: debug  ●  NOR` without touching the page title.
sees:
- **Left Ear (this page/card, borderless):** the screen's identity — such as a `chat.Thread` subject and `chat.Project`, `desk.Desk` (with active dealer focus like `focus tide`), `notes.Note` title, or `agents.Transcript` title + project + model — plus page-level markers (`pinned`, `approve push`, `working`, `stopped by you`, `stuck`). On `chat.Thread`, omits the model name (shown in `agents.Transcript` and `:switch model`) and omits redundant `waits · 4m` (the unread message right below already shows its age).
- **Center:** a solid square black mask framing the hardware camera notch so the top edge stays clean square geometry (`0 px` corners).
- **Right Ear (global status bar, full right width with subtle surface and crisp square left/bottom border):** the transient echo line (for ~4 seconds after an action, or until cleared), exception-only host notices (`<host> away` calmly in quiet, turning red only for `needs you` or an `away` host needed by the screen in view), the waiting lamp (`●` when something important within active dealer focus waits elsewhere), and the true input mode (`NOR` quiet, `INS` teal font color only — no background pill).

The lamp and chime stay off on `desk.Desk` and while dealer focus is `quiet`. Elsewhere, the card already in view does not light the lamp or ring the chime; any other waiting card matching the active dealer focus lights the lamp immediately. A card chimes once when it crosses the pressure line — after about 24 minutes waiting unseen while the window is awake, or immediately when a thread the user wrote in within ~38 minutes answers — rather than latching across the whole hand.

An `away` host is a normal everyday state: the window reconnects quietly in the background without flashing a red ticking counter across every screen. Only a host in `needs you` state (waiting for a security key touch, device enrollment, or refused trust), or an `away` host needed by the screen currently open, shows in red.

On the phone, a long name is cut at a word so that the card's words stay whole.

## concept Places
purpose: reach any place or remembered words in one prompt, and flip between recent places via pure MRU.
principle: the user presses `space` then `f` and hits `enter` to bounce back to the previous place, or types `clock` and sees both `Flaky clock test` and a message line mentioning a clock; picking the message jumps straight to that line.
sees: with nothing typed, recent places in pure MRU order (leaving out the current place; including any visited `agents.Transcript`, `agents.Terminal`, `agents.Shell`, or `agents.Desktop`, and any newly sent idea entered into MRU when sent), then what waits (left out on `desk.Desk`, where it is already in view). As the user types, best matches first across places and full-text matches. Searchable places are `chat.Thread` by subject, `chat.Project` (choosing one scopes `Places` to that project's threads), `browser.Page`, `browser.Bookmark`, `notes.Note`, `usage.Chart`, and people and channels reachable through `slack.Bot` (`status: not built`). Unvisited `agents.Workset` transcripts and unvisited `agents.Shell`, `agents.Terminal`, and `agents.Desktop` sessions do not clutter global `Places` search (open them from their thread; once visited they stay in MRU recents). A thread row shows its subject, its `deal.Card` words when it waits or the project's latest words otherwise (or `<host> away` when its host is away), then its project address. A full-text match row shows the matched line snippet and its thread or note. Among equal matches, the waiting thread comes first, then the most recently active: a thread the user visited or the project wrote to today comes before one from last week, and recency can lift a thread one step of match strength, never over a whole-name match. The current place ranks last among matching places. A place whose name starts with the typed word comes before a recent place that only contains it, and a match on words in order beats one out of order. The draft of the current thread is searched before the message history. Commands never appear in `Places`.
does:
- open `space` then `f` in `NOR` (on any screen, including `desk.Desk` and escaped raw places): open the places prompt; the screen underneath does not see the keys
- search: match every typed word anywhere in a row, in any order, and mark the matching letters; search the full text of messages and notes alongside place names. A query starting with `/` searches message and note text only; `chat:` and `note:` narrow text search to one kind; state prefixes `working:`, `stopped:`, `muted:`, and `snoozed:` filter threads by state; choosing a `chat.Project` scopes the prompt to that project's threads. The rows follow every keystroke without a pause the user can feel, on a history of ten thousand messages
- choose `up`, `down`, `ctrl-p`, `ctrl-n`, or click a row: move through or pick rows, wrapping at both ends without changing the typed words
- open `enter` or click: open the chosen place; opening a full-text message or note match jumps directly to that matched message or line (otherwise a thread opens by `chat.Thread.open`'s single rule: top `chat.Draft` if unseen messages arrived, else as left). With nothing typed, `enter` opens the top MRU place when there is one
rejected:
- mixing places and commands in one `:` prompt · 2026-11-26 · partial prefixes like `sto` or `qu` misfired `stop agent` and `quit` over matching threads; splitting `space f` (`Places`) and `:` (`Commands`) makes both safer and sharper
- object→action (`tab`) menus on rows in `Places` · 2026-11-26 · the user prefers opening a place to act on it rather than managing secondary submenus inside the finder
- listing every `agents.Workset` and unvisited terminal/shell/desktop in global `Places` search · 2026-11-26 · threads are the primary unit of work; open tools still enter MRU recents once visited

When nothing matches, `Places` offers no fallback rows. Nothing acts on a row before it is chosen.

## rule work has words before the agent writes any
when an agent works in a `chat.Thread` and the project has not written since the user did, then the thread's row in `Places` reads `working` (or `<host> away` if its host is away), or the first line of the agent's newest fresh `chat.Flash` when it has one; the word goes away when the agent stops or the thread waits.

## screen Places
answers: where can the user go, or which remembered line do they mean?
shows:
- the query in the `Places` prompt (and active project or state filter scope, if set), with the selected position and total at its right end
- the `Places` rows in match order, with `deal.Card` words or the project's latest words on thread rows, and matched snippets on text matches
states:
- empty: an empty query lists recent places in MRU order (leaving out the current place), then what waits; a query with no match shows `no match`
- loading: `searching…` if the text search is still running and no named rows match yet
- partial: named matches appear while the text search is still running
- error: not agreed
- full: the rows that match the query; on a phone, tap `previous` or `next` below a long list to page through the matches, then tap a row to open it
keys:
- `enter` or click: `Places.open`
- `up`, `down`, `ctrl-p`, `ctrl-n`: `Places.choose`
- `escape`: `Prompt`
starts: `Go to something by name`
captures: [message search on laptop](design/laptop-prompt.png), [blank phone palette](design/phone-prompt.png), and the phone with [text only](design/phone-palette-text.png).

## concept Commands
purpose: run any rho command by natural name or intent, with inline targets/arguments and live context, without ever colliding with place names.
principle: the user presses `:` and types `kill` — seeing `stop agent · tide · Flaky clock test · working (opus-high)` and `cancel agent request` ranked by intent — or types `:model sol-medium` to switch the current agent, or `:stop agent clock` to override the context target directly.
sees: with nothing typed, recently run commands pinned at the top, then all commands. Each command has a flat, natural verb-noun name with an explicit object noun (such as `compact agent`, `reset agent cache`, `rewind agent`, `rewind agent to here`, `stop agent`, `cancel agent request`, `set agent runtime`, `finish work`, `rename thread`, `fork thread`, `set project place`, `set project mode`, `set project runtime`, `rename project`, `retire project`, `new project`, `set dealer focus`, `clear dealer focus`, `attach host`, `detach host`, `list hosts`, `list devices`, `revoke device`, `open terminal`, `new terminal`, `open shell`, `close shell`, `open desktop`, `draw on desktop`, `open transcript`, `filter transcript`, `open thread`, `open page`, `bookmark page`, `remove bookmark`, `switch claude account`, `open usage`, `new note`, `list notes`, `write idea`, `pair device`, `show secret phrase`, `upload performance report`, `quit`) — never breadcrumb prefixes (`agent ›`, `project ›`, `hosts ›`) and never vague generic names (`compact`, `new cache key`). Each row shows the resolved target and current value from context (such as `tide · Flaky clock test · opus-high`, or `pick agent…` when ambiguous), plus its key sequence if it has one (name only on a narrow window). When the user types inline target or value words after the command, command words match first and the row visually separates the matched command from the parsed target/argument before `enter`. Every command defines weighted aliases/synonyms so natural phrasing matches what the user meant (for example, `kill` matches both `stop agent` and `cancel agent request` ranked by intent weight; `model` matches `set agent runtime` and `set project runtime`; `focus` / `quiet` / `dnd` match `set dealer focus`). Places and message text never appear in `Commands`.
does:
- open `:` in `NOR`, or `ctrl-shift-;` from a raw place or while typing: open the commands prompt; the screen underneath does not see the keys
- search: match typed words against command names and weighted aliases/synonyms, ranking stronger intent matches first and marking matched letters
- run with inline target or argument `enter`: typing a target override or value right after the command runs it in one shot without a follow-up `Prompt` (such as `:model sol-medium`, `:stop agent clock`, `:focus tide`, `:quiet 45m`, `:rename Fix clock race`, `:snooze 3h`, `:open https://…`); context target is the default shown on the row, and typed target words override it; omitting a required target or value opens the completion `Prompt`
- choose `up`, `down`, `ctrl-p`, `ctrl-n`, or click a row: move through or pick rows, wrapping at both ends without changing the typed words
- quit, from `Commands`: close the rho window
rejected:
- breadcrumb prefixes (`agent ›`, `project ›`, `hosts ›`, `usage ›`) · 2026-11-26 · flat verb-noun names with weighted aliases are faster to type and remember
- generic command names without object nouns (`compact`, `new cache key`) · 2026-11-26 · explicit nouns (`compact agent`, `reset agent cache`) stay clear as commands grow
- hiding commands behind screen-specific context · 2026-11-26 · commands stay globally reachable in `Commands`; when context is unambiguous the command uses it by default (overridable inline), otherwise it prompts for the target

## rule commands are global
when the user opens `Commands` from any screen, then commands are never hidden merely because of which screen is open; if a command's target (such as a `chat.Project`, `agents.Workset`, or `agents.Agent`) is unambiguous from the current screen it shows and uses it by default (unless the user overrides it inline), and otherwise marks the row `pick …` and opens a `Prompt` to ask for it.

## screen Commands
answers: what action can the user run here or globally?
shows:
- the query (and any inline target/argument) in the `Commands` prompt, with the selected position and total at its right end
- matching command rows in match order, each with its flat verb-noun name, visually distinct parsed inline target/argument, live target/current value, and quiet key sequence
states:
- empty: recently run commands at the top, followed by all commands; a query with no match shows `no match`
- loading: none; command list is local and instant
- partial: none
- error: invalid inline argument echoes the reason and keeps or reopens the prompt
- full: matching command rows
keys:
- `enter` or click: `Commands.run with inline target or argument`
- `up`, `down`, `ctrl-p`, `ctrl-n`: `Commands.choose`
- `escape`: `Prompt`
starts: `Run a command I rarely use`
captures: [commands only on phone](design/phone-palette-commands.png).

## concept Menu
purpose: offer a short list of keyed choices where the list itself is the information: the `space` leader (`Places`, `undo`, and card verdicts) and how long to snooze. Every other choice is in `Commands` or a `Prompt`.
principle: the user presses `space` on a card open in a thread; a block of rows appears along the bottom edge, each a key and a word; they press `s`, the block becomes the snooze times; they type `3` then `h`, and the card sleeps for three hours. The rows show the resolved times. On any non-card screen (including `desk.Desk`, `browser.Page`, `agents.Terminal`, `agents.Desktop`, `usage.Chart`), `space` opens the same leader menu with `f` places and (when undoable) `u` undo. On a `notes.Note`, `space` also offers `s` snooze for a device-only reminder.
sees: the menu's title, then its rows as key and words, laid out in columns; a row that holds a value shows the value.
does:
- pick a key, or click or tap a row: run the row; a row that opens another menu replaces this one
- count digits before a key: give the row a number, such as `3` before `d` for three days
- back `escape` or `ctrl-g`: return to the menu before, or close the menu

A menu with nothing in it does not open; the echo line reads its title and `nothing here`.

On the phone, a menu is a sheet of tall rows over the screen, with the key still shown at the end of each row. `‹ back` returns to the previous sheet. `close`, `cancel` at the bottom edge, or a tap in the dimmed area dismisses the whole menu without running a row or changing the work. A tap that starts outside a row cannot run it.

## concept Prompt
purpose: ask the user for one line of text, with completion, so a question never needs a dialog.
principle: the user runs `detach host` from `Commands`; the prompt line reads `detach host:` and lists the hosts; they press tab to complete one and enter to detach it.
sees: the question, such as `detach host:` or `runtime (sol-medium):`, the text typed so far, and below it the answers that fit, each with a word on what it is.
does:
- answer `enter`, or click/tap a row: take the highlighted answer, or the text typed
- next and previous `ctrl-n`, `ctrl-p`, or the arrows: move through the answers, wrapping at both ends
- delete word `ctrl-w`: delete the word before the cursor
- start of line `ctrl-a`: move to the start without changing the text
- clear `ctrl-u`: clear the line
- complete `tab`: put the highlighted answer into the line
- cancel `escape` or `ctrl-g`: close the prompt and change nothing

A prompt for a secret, such as the phrase that pairs devices, shows what is typed or pasted like any other prompt. Only rho's history of what was typed at prompts keeps nothing of it.

A privileged action from a host, such as whether an agent may push to `main`/`master`, arrives asynchronously inside its `chat.Thread` card rather than interrupting an open prompt; see `agents.Approve a push or a fetch`.

## concept Phone bar
status: phone ergonomics and thread interaction on mobile are currently on hold while desktop settles.
purpose: give the thumb what the keys give on the laptop.
principle: on the phone, the user reads the waiting message and taps `done` on the bar along the bottom edge; the echo line says it is done.
sees: one row of buttons along the bottom edge, each an icon and a word; which buttons show depends on the screen:
- on `desk.Desk`: the bar reaches the desk's actions by thumb; the exact buttons are not agreed
- with a card open in a thread: `places`, `menu`, `done`, `snooze`, and `more`; while writing, `undo` and `send` take their place
- from the workset transcript belonging to an open card: `places`, `done`, `snooze`, `more`, and `menu`
- reading elsewhere: `places`, `desk`, and `menu`; while writing, `undo` and `send` join
- while a prompt is open: `cancel` and `enter`
does:
- flick up from the end of a screen: deal the next card, as `deal.Card.deal` does
- flick down from the top of a screen: open the previous MRU place
- `menu`: open `Commands`
- `places`: open `Places`
- `more`: open the menu of `deal.Verdict`, as `space` does
- long press on a message: quote the pressed lines into the draft, as `chat.Draft.quote`

The snooze button opens the snooze times as a sheet, the same rows as on the laptop, each with its time.

## flow Go to something by name
runs: 100 a day
day: Reach anything.
when: the user knows the place they want, wants to flip to their last place, wants to check active/muted work, or remembers words from a message or note
want: reach it by typing what they remember (or in one `enter` for the last place)
so that: they never hunt through lists for it
steps:
1. the user opens `Places` with `space` then `f`
2. the user types part of the name or remembered words (or presses `enter` immediately for the previous MRU place)
   - if they choose to narrow the search, the user adds a prefix (`/`, `chat:`, `note:`, `working:`, `stopped:`, `muted:`, `snoozed:`) or picks a `chat.Project` to scope to its threads
3. the user opens the match with `Places.open`; a full-text match jumps straight to the matched message or line
system: keeps pure MRU recents, ranks matches by what the user typed, searches messages and notes alongside places, filters by state prefix, and jumps to the matched line on full-text open.
user: the name, state filter, or words they remember.
cases: two threads with the same subject in different projects; the user remembers a line of a message deep in a long thread but not its subject; checking `working:` threads or recovering a `muted:` thread; bouncing between two places via `space f` + `enter`.

## flow Run a command I rarely use
runs: 5 a day
day: Reach anything.
when: the user wants an action such as steering an agent, changing a project setting, focusing the dealer, attaching a host, or opening a terminal
want: say what they mean and run it (optionally overriding the target or passing an argument inline)
so that: the user does not need to remember a key, a hierarchy prefix, or be on a specific screen first
steps:
1. the user opens `Commands` with `:` (or `ctrl-shift-;` while typing or in a raw place)
2. the user types a few letters of the command or an alias (optionally followed by an inline target or argument like `model sol-medium`, `stop agent clock`, `focus tide`, or `quiet 45m`)
3. the user presses `enter`; the command runs against the live target shown on the row
4. if the command still needs a target or value, the user answers the `Prompt` that follows
system: keeps commands globally available in `Commands`, ranks weighted aliases by intent, shows live targets and current values on rows, visually splits command from inline target/argument, and remembers recent commands at the top of an empty `:`.
user: the command's name or alias, and any target or argument it needs.
cases: running a project or agent command from `desk.Desk`; typing `kill` and choosing between `stop agent` and `cancel agent request`; overriding context with `:stop agent clock`; passing `:rename Fix clock race` inline; on the phone, tapping a command below the first row.

## rule a menu or prompt takes the keys
when a `Menu` or a `Prompt` is open, then every key goes to it, and the `Screen` under it does not move until it closes.

## flow Report a slow window
runs: 1 a month
day: Keep the work moving.
when: the window stutters or freezes for a moment, and the user wants it fixed
want: send what it was doing to a place an agent can read
so that: an agent can find the cause without the user having to put it into words
steps:
1. the user opens `Commands` and chooses `upload performance report`
2. the echo line reads `uploading a … KiB performance report to …`, then `performance report stored on … at …`
system: keeps a record of the last half minute of how the window drew and what it worked on, all the time; sends it to the attached host whose name comes first.
user: the moment to send it, soon after the stutter.
cases: no host attached, where the echo line reads `performance report: no host is attached`
