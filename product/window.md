# window

The one window that every screen sits in: one screen at a time, the line at the top that says where the user is, and the ways to reach any command, any place, and any earlier screen by keys or by thumb.

## concept Screen
purpose: show one thing at a time, so the user is never looking at two things that compete for them.
principle: the user opens a room; it fills the window; they open a thread from it, and the thread fills the window; `Screen.back` returns to the room exactly as they left it.
sees: the screen's text, filling the window under the `Status line`; nothing around it.
does:
- back `ctrl-k`: go to the screen before, as it was left
- forward `ctrl-j`: go to the screen after; from the newest screen, deal the next card, as `deal.Card.deal` does
- close `q`: close this screen; no back or forward reaches it again
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
sees: the commands for the screen in view and for the thing under the cursor, each with the keys of its menu beside it; a command inside a menu shows its path, such as `hosts › attach`.
does:
- open `:`: list the commands, best match first as the user types
- run `enter`: run the command; a command that is a `Menu` opens that menu

The keys shown beside a command are the keys inside its menu, such as `a` in `hosts › attach` after `hosts` is open. A command at the top of the list, such as `usage…`, shows a key such as `shift-u` that does something else when pressed outside the palette.

## concept Menu
purpose: offer a short list of choices where the list itself is the information, such as how long to snooze or which agent to join.
principle: the user presses `space` on a card; a block of rows appears along the bottom edge, each a key and a word; they press `s`, the block becomes the snooze times; they type `45` then `m`, and the card sleeps for 45 minutes.
sees: the menu's title, then its rows as key and words, laid out in columns; a row that holds a value shows the value.
does:
- pick a key, or click or tap a row: run the row; a row that opens another menu replaces this one
- count digits before a key: give the row a number, such as `3` before `d` for three days
- back `escape`: return to the menu before, or close the menu

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

This is the minibuffer. At a prompt for a secret, such as a Slack token, rho's history of what was typed at prompts keeps nothing.

## concept Find
purpose: go to anything by its name, so nothing needs a place on screen to be reachable.
principle: the user presses `ctrl-shift-f` and types `privacy`; the thread `privacy VM` is in the list, with its channel beside it; enter opens it.
sees: every channel, thread, agent, note, and Slack conversation and Slack thread, by title, newest first, with where each is and, for an agent, what it is doing, such as `rho · waiting`.
does:
- find `ctrl-shift-f`: open the prompt `find:`, which narrows the list as the user types
- open `enter`: open the thing chosen

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
