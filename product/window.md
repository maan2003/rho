# window

The one window that every screen sits in: one screen at a time, the line at the top that says where the user is, and the ways to reach any command, any place, and any earlier screen by keys or by thumb.

## bet An editor, not an app
instead of: an app with dialogs, buttons, and a different set of controls on each screen.
serves: Reach anything.
because: the user spends eight to ten hours a day here and drives everything with keys; one set of motions, search, and modes on every screen pays back over months.
costs: the first weeks are harder, and the phone must be given the keys' work another way.

## bet One window becomes the operating system
instead of: rho beside a terminal, a browser, and other apps on the laptop.
serves: Reach anything.
because: everything the user does should be a place with the same keys and the same way back; terminals, desktops, and the browser come inside, and what the user uses most becomes a place of its own.
costs: what has no place yet is reached through a terminal, the browser, or an agent.

## bet One place fills the window
instead of: panes, tabs, or a sidebar always on screen.
serves: Know what wants them, and where they were.
because: the user works in one place for an hour at a time, and nothing else should pull at them; two places side by side is rare power, used on purpose.
costs: two things the user wants to see together are two places, one key apart.

## bet The phone is the same window, laid out smaller
instead of: a phone app with its own screens.
serves: Reach anything.
because: the phone is as powerful as the laptop, and every job must be possible there; the same places and the same stack, with a bar of buttons for the thumb.
costs: a prompt or a palette that is easy with keys is slow by thumb.

## bet Defaults are written in the code, not set by the user
instead of: settings screens.
serves: Know what wants them, and where they were.
because: rho has one user; what they do is observed, and the decision is written into the product once, so there is nothing to configure and nothing that adapts on its own.
costs: changing a default is a change to the product.

## concept Screen
purpose: show one thing at a time, so the user is never looking at two things that compete for them.
principle: the user opens a room; it fills the window; they open a thread from it, and the thread fills the window; `Screen.back` returns to the room exactly as they left it.
sees: the screen's text, filling the window under the `Status line`; nothing around it.
does:
- back `ctrl-k`: go to the screen before, as it was left; on the oldest screen the echo line reads `at the oldest`
- forward `ctrl-j`: go to the screen after; from the newest screen, deal the next card, as `deal.Card.deal` does
- close `q`, or `ctrl-shift-backspace` while typing: close this screen; no back or forward reaches it again
- close and deal `ctrl-shift-j`: close this screen and deal the next card
- home `tab`: go to `home.Home`; on Home, go back to the screen before

Every screen is text in an editor. The Vim motions, search, and modes work on every screen, so the user moves through a list of rooms the same way they move through a thread or a chart's numbers.

## concept Status line
purpose: tell the user, at a glance, where they are and what is wrong, without a screen of its own.
principle: the user opens a thread that asks for them; the line at the top names it and says `asks · 2h`; a host drops; the line says `disconnected` in red, with the host's name when more than one is attached, until it is back.
sees: where the user is: the room and the thing under the cursor, and when that thing is a card, its words beside it, such as `asks · 48.4d` or `result · 5.4h ago`; the `deal.Lamp`; a host that is not connected, in red; the Vim mode, such as `NOR` or `INS`.

On the phone, a long name is cut at a word so that the card's words stay whole.

## concept Echo line
purpose: answer the key the user just pressed, so they know what happened.
principle: the user says done on a card; the top line reads `done: debug` for a few seconds, then shows where they are again.
sees: one line of words in the place of the `Status line`, for about four seconds.

The echo line is also where rho says that something failed, for example that a host refused to start an agent.

## concept Palette
purpose: reach every command by its name, so no command needs a key the user must remember.
principle: the user presses `:`; the prompt line reads `command:` and lists every command that fits this screen; they type `sa`, `start agent here` comes first, and enter runs it.
sees: `home`, `find`, `undo`, `hosts`, and `quit` on every screen, mixed with the commands for the screen in view and for the thing under the cursor; with nothing typed, the shorter names come first; each row has the keys of its menu beside it; a command inside a menu shows its path, such as `hosts › attach`.
does:
- open `:`: list the commands, best match first as the user types
- run `enter`: run the command; a command that is a `Menu` opens that menu; a name that fits no command echoes `no command …`

The keys shown beside a command are the keys inside its menu, such as `a` in `hosts › attach` after `hosts` is open. A command at the top of the list shows a key too, such as `shift-u` beside `usage…` or `a` beside `start agent here`, but no menu holds the top of the list, so outside the palette that key does something else or nothing.

## concept Menu
purpose: offer a short list of choices where the list itself is the information, such as how long to snooze or which agent to join.
principle: the user presses `space` on a card; a block of rows appears along the bottom edge, each a key and a word; they press `s`, the block becomes the snooze times; they type `45` then `m`, and the card sleeps for 45 minutes.
sees: the menu's title, then its rows as key and words, laid out in columns; a row that holds a value shows the value.
does:
- pick a key, or click or tap a row: run the row; a row that opens another menu replaces this one
- count digits before a key: give the row a number, such as `3` before `d` for three days
- back `escape` or `ctrl-g`: return to the menu before, or close the menu

A menu with nothing in it does not open; the echo line reads its title and `nothing here`.

On the phone, a menu is a sheet of tall rows over the screen, with `‹ back` and `close` at its top, and the key still shown at the end of each row.

## concept Prompt
purpose: ask the user for one line of text, with completion, so a question never needs a dialog.
principle: the user chooses `hosts › detach`; the prompt line reads `detach host:` and lists the hosts; they press tab to complete one and enter to detach it.
sees: the question, such as `find:` or `runtime (sol-medium):`, the text typed so far, and below it the answers that fit, each with a word on what it is.
does:
- answer `enter`: take the highlighted answer, or the text typed
- next and previous `ctrl-n`, `ctrl-p`, or the arrows: move through the answers
- complete `tab`: put the highlighted answer into the line
- cancel `escape` or `ctrl-g`: close the prompt and change nothing

A prompt for a secret, such as a Slack token, shows what is typed or pasted like any other prompt. Only rho's history of what was typed at prompts keeps nothing of it.

A question from a host, such as whether an agent may push, opens a prompt by itself, over whatever the user is doing; see `agents.Approve a push or a fetch`.

## concept Find
purpose: go to anything by its name, so nothing needs a place on screen to be reachable.
principle: the user presses `ctrl-shift-f` and types `privacy`; the thread `privacy VM` is in the list, with its channel beside it; enter opens it.
sees: every channel, thread, agent, agent's desktop, note, usage chart, and Slack conversation and Slack thread, by title, newest first, with where each is and, for an agent, what it is doing, such as `rho · waiting`; at most 40 at a time.
does:
- find `ctrl-shift-f`: open the prompt `find:`, which narrows the list as the user types
- open `enter`: open the thing chosen; a name that fits nothing echoes `find: nothing called …`

## concept Phone bar
purpose: give the thumb what the keys give on the laptop.
principle: on the phone, Home shows the rooms and a bar along the bottom edge; the user puts the cursor on a room, taps `done`, and the echo line says it is done.
sees: one row of buttons along the bottom edge, each an icon and a word; which buttons show depends on the screen:
- on `home.Home`: `back` and `menu`, and, while the cursor is on a room, `open`, `done`, `mute`, `snooze`, and `more`
- on any other screen: `back`, `home`, `menu`, `more` while the cursor is on something a verdict can land on, and `send` when the screen has a message box
- while a prompt is open: `cancel` and `enter`
does:
- flick up from the end of a screen: forward, and from the newest screen deal the next card, as `Screen.forward` does
- flick down from the top of a screen: back, as `Screen.back` does
- `menu`: open the `Palette`
- `more`: open the verdict menu, as `space` does

The snooze button opens the snooze times as a sheet. A thumb has no way to type a count there, so a tap snoozes for one minute, one hour, one day, or one week.

## job Go to something by name
when: I know what I want to look at, and it is not the card in front of me
want: reach it by typing part of its name
so that: I never hunt through lists for it
flow:
1. `Find.find`; the prompt `find:` lists everything, newest first
2. type part of the name; the list narrows
3. `Find.open`; the thing fills the window
system: keeps every name of every system in one list, and puts the most recently active first.
user: the words of the name.
cases: two threads with the same name in different channels; an agent whose workset name I do not remember, only its short code

## job Run a command I rarely use
when: I want something rare, such as a usage chart, renaming a thread, or attaching a host
want: say its name and run it
so that: I do not need to remember a key for it
flow:
1. open the `Palette` with `:`
2. type a few letters of the command
3. `enter`; the command runs, or its `Menu` opens
4. answer the menu or the `Prompt` that follows
system: shows only the commands that fit the screen and the thing under the cursor.
user: the command's name, and any answer it asks for.
cases: on the phone, where the list of commands cannot be tapped and the user must type or take the first one

## rule a menu or prompt takes the keys
when a `Menu` or a `Prompt` is open, then every key goes to it, and the `Screen` under it does not move until it closes.

## job Report a slow window
when: the window stutters or freezes for a moment, and I want it fixed
want: send what it was doing to a place an agent can read
so that: an agent can find the cause without my own words for it
flow:
1. open the `Palette` and choose `upload performance report`
2. the echo line reads `uploading a … KiB performance report to …`, then `performance report stored on … at …`
system: keeps a record of the last half minute of how the window drew and what it worked on, all the time; sends it to the attached host whose name comes first.
user: the moment to send it, soon after the stutter.
cases: no host attached, where the echo line reads `performance report: no host is attached`
