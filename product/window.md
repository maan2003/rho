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

Every screen is text in an editor. Helix's selections, motions, search, and modes work on every screen, so the user moves through a list of threads the same way they move through a thread or a chart's numbers. `/` and `?` ask `search:`, `n` and `N` go on, a search wraps round the end and says so, and `G` goes to the end.

## concept Status line
purpose: tell the user, at a glance, where they are, what just happened, and whether something important waits elsewhere, without a screen of its own.
principle: the user opens a thread that asks for them; the line at the top names it and says `asks · 2h`; they say done, and for a few seconds the line reads `done: debug`; later, deep in an agent's screen, a yellow dot lights at its right end; they finish and deal.
sees: where the user is: the place, such as a `mail.Project` and `mail.Thread`, and the thing under the cursor, and when the screen is that card, its words beside it, such as `asks · 48.4d` or `result · 5.4h ago`; the lamp, a yellow dot `●` at the right end, while something important waits and the user looks at something else; a host that is not connected, in red; the Helix mode, such as `NOR` or `INS`; and, for about four seconds after a key, the echo line in its place: one line of words that answers the key, or says that something failed, such as that a host refused to start an agent.

The lamp stays off on `desk.Desk`. Elsewhere, the card already in view does not light the lamp or ring the chime; another waiting card can. When that card rises past a higher line, the window chimes once.

When a host drops, the line names it and says `reconnecting · 12s` in red, counting from the drop, until it is back; the echo line says why once, `rho: disconnected · connection lost`, or `cannot reach the host` when it never answered.

On the phone, a long name is cut at a word so that the card's words stay whole.

## concept Palette
status: one prompt lists commands, places and notes, and a phone tap chooses a row. Full-text search across mail and notes is not built.
purpose: reach any place, command, or remembered words in one prompt.
principle: the user opens `:` and types `clock`; the matching subject, `Flaky clock test`, and a mail that mentions a clock are both in reach. The user chooses the one they meant.
sees: best matches first as the user types; with nothing typed, recent places newest first (leaving out the current place), then what waits, then the screen's commands. Places include `mail.Thread` by subject, `mail.Project`, people and channels reachable through `slack.Bot`, `browser.Page`, `browser.Bookmark`, `notes.Note`, and `usage.Chart`. A thread row shows its subject, its `deal.Card` words when it waits or its `mail.Status` otherwise, then its project address. Among equal matches, the waiting thread comes first, then the most recently visited. The current place ranks last among matching places. A command whose whole name matches comes first. When each typed word starts a word in a command's name, that command also comes before partial place matches, including matches inside words. A place with a matching full name still comes before a command that merely contains it. Other commands follow the places. Each command appears once, by its own name, not the name of the thing under the cursor. A command shows the entire key sequence that works from this screen, if it has one.
does:
- open `:`, or `ctrl-shift-;` from a terminal, a desktop, a page, or a draft the user is typing: open the same prompt; the place under it does not see the keys
- search: match every typed word anywhere in a row, in any order, and mark the matching letters; search the full text of mail and notes, with optional prefixes to narrow the search
- run `enter`: open the chosen place or run the chosen command; a command that is a `Menu` opens that menu. With nothing typed, `enter` opens the last place when there is one

When nothing matches, the prompt offers no fallback rows. Nothing acts on a row before it is chosen; the idea key belongs to `mail.Write an idea`, not to the palette.

## screen Palette
status: full-text search across mail and notes is not built; the matching mail line in the mock does not appear.
answers: where can the user go, or what can they run, with these words?
shows:
- the query in the `Palette` prompt, with the selected position and total at its right end
- the `Palette` rows in match order, with each command's key and the `mail.Status` or `deal.Card` words where a thread has them
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

Flaky clock test         asks · 2.0h                  tide@lab
Cache eviction tests     Running the clock tests    tide@lab
Mail · Flaky clock test  The mock clock is shared between tests.
```

## concept Menu
purpose: offer a short list of choices where the list itself is the information, such as how long to snooze or which agent to join.
principle: the user presses `space` on a card open in a thread; a block of rows appears along the bottom edge, each a key and a word; they press `s`, the block becomes the snooze times; they type `45` then `m`, and the card sleeps for 45 minutes.
sees: the menu's title, then its rows as key and words, laid out in columns; a row that holds a value shows the value.
does:
- pick a key, or click or tap a row: run the row; a row that opens another menu replaces this one
- count digits before a key: give the row a number, such as `3` before `d` for three days
- back `escape` or `ctrl-g`: return to the menu before, or close the menu

A menu with nothing in it does not open; the echo line reads its title and `nothing here`.

On the phone, a menu is a sheet of tall rows over the screen, with the key still shown at the end of each row. `‹ back` returns to the previous sheet. `close`, `cancel` at the bottom edge, or a tap in the dimmed area dismisses the whole menu without running a row or changing the work. A tap that starts outside a row cannot run it.

## concept Prompt
purpose: ask the user for one line of text, with completion, so a question never needs a dialog.
principle: the user chooses `hosts › detach`; the prompt line reads `detach host:` and lists the hosts; they press tab to complete one and enter to detach it.
sees: the question, such as `detach host:` or `runtime (sol-medium):`, the text typed so far, and below it the answers that fit, each with a word on what it is.
does:
- answer `enter`: take the highlighted answer, or the text typed
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
- with a card open in a thread: `back`, `menu`, `done`, `snooze`, and `more`; `send` appears only when there is a draft
- from the agent transcript belonging to an open card: `back`, `done`, `snooze`, `more`, and `menu`
- reading elsewhere: `back`, `desk`, and `menu`; `send` appears only when there is a draft
- while a prompt is open: `cancel` and `enter`
does:
- flick up from the end of a screen: forward, and from the newest screen deal the next card, as `Screen.forward` does
- flick down from the top of a screen: back, as `Screen.back` does
- `menu`: open the `Palette`
- `more`: open the menu of `deal.Verdict`, as `space` does

The snooze button opens the snooze times as a sheet. A thumb has no way to type a count there, so a tap snoozes for one minute, one hour, one day, or one week.

## flow Go to something by name
status: the prompt reaches places by name; it cannot search the full text of mails or notes yet.
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
3. the user presses `enter`; the command runs, or its `Menu` opens
4. the user answers the menu or the `Prompt` that follows
system: shows only the commands that fit the screen and the thing under the cursor.
user: the command's name, and any answer it asks for.
cases: on the phone, tapping a command below the first row
reviewed: 2026-11-20 · laptop · reviewer · 2 findings

## rule a menu or prompt takes the keys
when a `Menu` or a `Prompt` is open, then every key goes to it, and the `Screen` under it does not move until it closes.

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
