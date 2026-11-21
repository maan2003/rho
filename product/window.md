# window

The one window that every screen sits in: one screen at a time, the line at the top that says where the user is, and the ways to reach any command, any place, and any earlier screen by keys or by thumb.

## bet One window becomes the operating system
instead of: rho beside a terminal, a browser, and other apps on the laptop.
serves: Reach anything.
because: everything the user does should be a place with the same keys and the same way back; terminals, desktops, and the browser come inside, and what the user uses most becomes a place of its own.
costs: what has no place yet is reached through a terminal, the browser, or an agent.

## concept Screen
purpose: show one thing at a time, so the user is never looking at two things that compete for them.
principle: the user opens a `mail.Thread` from the desk; `Screen.back` returns to the desk exactly as they left it, narrowed or not.
sees: the screen's text, filling the window under the `Status line`; nothing around it.
does:
- back `ctrl-k`: go to the screen before, as it was left; on the oldest screen the echo line reads `at the oldest`
- forward `ctrl-j`: go to the screen after; from the newest screen, deal the next card, as `deal.Card.deal` does
- close `q`, or `ctrl-shift-backspace` while typing: close this screen; no back or forward reaches it again
- close and deal `ctrl-shift-j`: close this screen and deal the next card
- desk `tab`: go to `desk.Desk`; on the desk, go back to the screen before

The way back keeps the user's earlier screens, as they were left. A screen that can no longer open is passed over. Returning after more than ten minutes opens the desk; see `desk.a cold return opens the desk`.

Every screen is text in an editor. Helix's selections, motions, search, and modes work on every screen, so the user moves through a list of threads the same way they move through a thread or a chart's numbers. `/` and `?` ask `search:`, `n` and `N` go on, a search wraps round the end and says so, and `G` goes to the end. A search selects the whole match, so `c` changes it and `d` deletes it.

## concept Edge-stuck excerpt
purpose: let the user keep working in one part of the text while reading the rest of it.
principle: the user scrolls down to read an old `mail.Mail` while typing a `mail.Draft`; the draft remains editable at the top edge, with history below it.
sees: a designated excerpt as ordinary text while any of it is in the window; when it has left the window past one edge, the excerpt sits over that edge, the top for a `mail.Draft`. It can take up to half the window; if taller, its text scrolls inside that space.
does:
- scroll the rest `ctrl-u`, `ctrl-d`, or by thumb outside the excerpt: move the view, not the point
- move and search: use the same motions as one buffer; `j` from the last line of a top excerpt moves into the text below it, `G` goes to the end

## concept Status line
purpose: tell the user, at a glance, where they are, what just happened, and whether something important waits elsewhere, without a screen of its own.
principle: the user opens a thread that asks for them; the line at the top names it and says `waits · 2h`; they say done, and for a few seconds the line reads `done: debug`; later, deep in an agent's screen, a yellow dot lights at its right end; they finish and deal.
sees: where the user is: the place, such as a `mail.Project` and `mail.Thread`, and the thing under the cursor, and when the screen is that card, its words beside it, such as `waits · 2h`; the lamp, a yellow dot `●` at the right end, while something important waits and the user looks at something else; a host that is not connected, in red; the Helix mode, such as `NOR` or `INS`; and, for about four seconds after a key, the echo line in its place: one line of words that answers the key, or says that something failed, such as that a host refused to start an agent.

The lamp stays off on `desk.Desk`. Elsewhere, the card already in view does not light the lamp or ring the chime; another waiting card can. When that card rises past a higher line, the window chimes once.

When a host drops, the line names it and says `reconnecting · 12s` in red, counting from the drop, until it is back; the echo line says why once, `rho: disconnected · connection lost`, or `cannot reach the host` when it never answered.

On the phone, a long name is cut at a word so that the card's words stay whole.

## concept Palette
status: the built palette ranks places and commands as below but without the recency bonus and without the `>` and `/` prefixes, and its text search is slow on a large mail history.
purpose: reach any place, command, or remembered words in one prompt.
principle: the user opens `:` and types `clock`; the matching subject, `Flaky clock test`, and a mail that mentions a clock are both in reach. The user chooses the one they meant.
sees: best matches first as the user types; with nothing typed, recent places newest first (leaving out the current place), then what waits (left out on `desk.Desk`, where it is already in view), then the screen's commands. Places include `mail.Thread` by subject, `mail.Project`, people and channels reachable through `slack.Bot`, `browser.Page`, `browser.Bookmark`, `notes.Note`, and `usage.Chart`. A thread row shows its subject, its `deal.Card` words when it waits or the project's latest words otherwise, then its project address. Among equal matches, the waiting thread comes first, then the most recently active: a thread the user visited or the project wrote to today comes before one from last week, and recency can lift a thread one step of match strength, never over a whole-name match or a command. The current place ranks last among matching places. A place whose name starts with the typed word comes before a recent place that only contains it, and a match on words in order beats one out of order. A command whose whole name matches comes first. When each typed word starts a word in a command's name, that command also comes before partial place matches, including matches inside words. A place with a matching full name still comes before a command that merely contains it. Other commands follow the places. Each command appears once, by its own name, not the name of the thing under the cursor. A command shows the entire key sequence that works from this screen, if it has one; on a narrow window it shows its name only. The draft of the current thread is searched before the mail history.
does:
- open `:`, or `ctrl-shift-;` from a terminal, a desktop, a page, or a draft the user is typing: open the same prompt; the place under it does not see the keys
- search: match every typed word anywhere in a row, in any order, and mark the matching letters; search the full text of mail and notes. A query that starts with `>` lists commands only; one that starts with `/` searches the text of mail and notes only; `mail:` and `note:` narrow the text search to one kind. The rows follow every keystroke without a pause the user can feel, on a history of ten thousand mails
- run `enter`: open the chosen place or run the chosen command; a command that needs an answer opens a `Prompt`. With nothing typed, `enter` opens the last place when there is one
rejected:
- menus for hosts, usage, and starting an agent · 2026-11-20 · each row is a palette row by its full name, such as `hosts › attach`; the palette is enough
- a second key for threads, apart from `:` for commands · 2026-11-21 · one prompt with a `>` prefix keeps one habit; commands already rank first when the typed words start their name

When nothing matches, the prompt offers no fallback rows. Nothing acts on a row before it is chosen; the idea key belongs to `mail.Write an idea`, not to the palette.

## screen Palette
answers: where can the user go, or what can they run, with these words?
shows:
- the query in the `Palette` prompt, with the selected position and total at its right end
- the `Palette` rows in match order, with each command's key and the `deal.Card` words or the project's latest words where a thread has them
states:
- empty: an empty query lists recent places newest first, leaving out the current place, then what waits, then the screen's commands; a query with no match shows `no match`
- loading: not agreed
- partial: not agreed
- error: not agreed
- full: the rows that match the query
keys:
- `enter`: `Palette.run`
- `ctrl-n`, `ctrl-p`, `escape`: `Prompt`
starts: `Go to something by name`, `Run a command I rarely use`

Built renders: [filtered laptop palette](design/laptop-prompt.png) and [blank phone palette](design/phone-prompt.png).

mock:
```text
: clock

Flaky clock test         waits · 2.0h                 tide@lab
Cache eviction tests     running the clock tests    tide@lab
Mail · Flaky clock test  The mock clock is shared between tests.
```

## concept Menu
purpose: offer a short list of choices where the list itself is the information: the verdicts, and how long to snooze. Every other choice is a `Palette` row or a `Prompt`.
principle: the user presses `space` on a card open in a thread; a block of rows appears along the bottom edge, each a key and a word; they press `s`, the block becomes the snooze times; they type `3` then `h`, and the card sleeps for three hours. The rows show the resolved times.
sees: the menu's title, then its rows as key and words, laid out in columns; a row that holds a value shows the value.
does:
- pick a key, or click or tap a row: run the row; a row that opens another menu replaces this one
- count digits before a key: give the row a number, such as `3` before `d` for three days
- back `escape` or `ctrl-g`: return to the menu before, or close the menu

A menu with nothing in it does not open; the echo line reads its title and `nothing here`.

On the phone, a menu is a sheet of tall rows over the screen, with the key still shown at the end of each row. `‹ back` returns to the previous sheet. `close`, `cancel` at the bottom edge, or a tap in the dimmed area dismisses the whole menu without running a row or changing the work. A tap that starts outside a row cannot run it.

## concept Prompt
purpose: ask the user for one line of text, with completion, so a question never needs a dialog.
principle: the user runs `hosts › detach` from the palette; the prompt line reads `detach host:` and lists the hosts; they press tab to complete one and enter to detach it.
sees: the question, such as `detach host:` or `runtime (sol-medium):`, the text typed so far, and below it the answers that fit, each with a word on what it is.
does:
- answer `enter`, or a tap on a row on the phone: take the highlighted answer, or the text typed
- next and previous `ctrl-n`, `ctrl-p`, or the arrows: move through the answers (the Palette uses `up` to recall the last query)
- delete word `ctrl-w`: delete the word before the cursor
- start of line `ctrl-a`: move to the start without changing the text
- clear `ctrl-u`: clear the line
- recall `up`: put the last Palette query into the line
- complete `tab`: put the highlighted answer into the line
- cancel `escape` or `ctrl-g`: close the prompt and change nothing

A prompt for a secret, such as the phrase that pairs devices, shows what is typed or pasted like any other prompt. Only rho's history of what was typed at prompts keeps nothing of it.

A question from a host, such as whether an agent may push, opens a prompt by itself, over whatever the user is doing; see `agents.Approve a push or a fetch`.

## concept Phone bar
purpose: give the thumb what the keys give on the laptop.
principle: on the phone, the user reads the waiting mail and taps `done` on the bar along the bottom edge; the echo line says it is done.
sees: one row of buttons along the bottom edge, each an icon and a word; which buttons show depends on the screen:
- on `desk.Desk`: the bar reaches the desk's actions by thumb; the exact buttons are not agreed
- with a card open in a thread: `back`, `menu`, `done`, `snooze`, and `more`; while writing, `undo` and `send` take their place
- from the agent transcript belonging to an open card: `back`, `done`, `snooze`, `more`, and `menu`
- reading elsewhere: `back`, `desk`, and `menu`; while writing, `undo` and `send` join
- while a prompt is open: `cancel` and `enter`
does:
- flick up from the end of a screen: forward, and from the newest screen deal the next card, as `Screen.forward` does
- flick down from the top of a screen: back, as `Screen.back` does
- `menu`: open the `Palette`
- `more`: open the menu of `deal.Verdict`, as `space` does
- long press on a mail: quote the pressed lines into the draft, as `mail.Draft.quote`

The snooze button opens the snooze times as a sheet, the same rows as on the laptop, each with its time.

## flow Go to something by name
runs: 20 a day
day: Reach anything.
when: the user knows the place they want or remembers words from a mail or note
want: reach it by typing what they remember
so that: they never hunt through lists for it
steps:
1. the user opens the `Palette` with `:`
2. the user types part of the name or remembered words
   - if they choose to narrow the search, the user adds a prefix
3. the user opens the match with `Palette.run`
system: ranks matches by what the user typed and searches mail and notes in the same prompt as places and commands.
user: the name or the words they remember.
cases: two threads with the same subject in different projects; the user remembers a line of a mail but not its subject.
reviewed: 2026-11-20 · laptop · reviewer · 1 finding

## flow Run a command I rarely use
runs: 2 a day
day: Reach anything.
when: the user wants something rare, such as a usage chart, renaming a thread, or attaching a host
want: say its name and run it
so that: the user does not need to remember a key for it
steps:
1. the user opens the `Palette` with `:`
2. the user types a few letters of the command
3. the user presses `enter`; the command runs
4. the user answers the `Prompt` that follows, if it asks for one
system: shows only the commands that fit the screen and the thing under the cursor.
user: the command's name, and any answer it asks for.
cases: on the phone, tapping a command below the first row
reviewed: 2026-11-20 · laptop · reviewer · 2 findings

## rule a menu or prompt takes the keys
when a `Menu` or a `Prompt` is open, then every key goes to it, and the `Screen` under it does not move until it closes; `ctrl-k` inside a prompt does nothing.

## flow Report a slow window
runs: 1 a month
day: Keep the work moving.
when: the window stutters or freezes for a moment, and the user wants it fixed
want: send what it was doing to a place an agent can read
so that: an agent can find the cause without the user having to put it into words
steps:
1. the user opens the `Palette` and chooses `upload performance report`
2. the echo line reads `uploading a … KiB performance report to …`, then `performance report stored on … at …`
system: keeps a record of the last half minute of how the window drew and what it worked on, all the time; sends it to the attached host whose name comes first.
user: the moment to send it, soon after the stutter.
cases: no host attached, where the echo line reads `performance report: no host is attached`
