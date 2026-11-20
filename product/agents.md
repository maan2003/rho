# agents

The agents do the work. The user starts one from a message, talks to it in its thread, and looks into what it does: its transcript, its terminals, its shell, and its desktops. Agents have no list; each is reached from the thread it works in, or by name, and its screen is its transcript.

## bet An agent is labour, not a colleague
instead of: a list of agents with profiles and direct messages, like people in a contact list.
serves: Keep the work moving.
because: the user has dozens of agents and cares about the work, not the worker; the thread is where the work is, so the agent is reached from there or by name.
costs: an agent has a code for a name, and the user must map it to the work it does.

## bet Trust by inspection
instead of: agents that report what they did, which the user believes.
serves: Keep the work moving.
because: the user does not edit code by hand and must still know whether a result is right; the transcript shows every step, and the terminal, shell, and desktop let the user stand beside the agent.
costs: a transcript is long, and the user reads it to find what went wrong.

## journey An agent in trouble gets back on track
goal: the user gets the agent back to work so its result comes.
starts when: an agent stops or goes quiet.
steps:
1. the user learns of it without looking for it, `deal.Clear what waits`
2. the user sees why in one glance, `Look into what an agent did`
3. the user steers it, `Steer an agent`
   - or the user tells it what to do in its thread, `chat.Answer an agent`
4. the user gets the result after the agent carries on, `deal.Clear what waits`
done when: the user sees the agent's result.
day: Keep the work moving.

## concept Agent
purpose: do a piece of the user's work on a host, in a thread the user can read and answer.
principle: the user starts an agent from the message `fix the flaky test`; the agent joins that thread, reads the message first, works in its workset, and posts there; when it asks, the thread becomes a card.
sees: its name, which is its workset's name with its short code, such as `manifold-release-and-bridge (eng-kjhi)`, or the short code alone, such as `eng-q9tz`; its host; its folder on the host, such as `/src/rho`; what it is doing: `working`, `waiting`, `idle`, `failed`, `retrying`, or `archived`.
has:
- many `Terminal`
- one `Shell`
- many `Desktop`
- many `chat.Thread`, the one it started from first
does:
- open: from the `with` line of a thread, or by name with `window.Palette`; its screen, `Agent`, opens at its newest step
- open its thread, from the palette as `open its thread`: open the thread it started from
- rewind turns, from the palette: take back its last turn; the palette has no way to give a count, so it is always one
- compact context, from the palette: shorten what it holds in mind
- new cache key, from the palette: make it start its model's cache afresh
- set runtime, from the palette: ask `runtime (current):` and switch the model it runs on
- cancel the request, from the palette: stop what it is asking its model now

While the agent's host is away, each of these echoes `the agent's host is not attached`.

In a thread, a message from an agent is signed with its long code, such as `q9tz45g2bcvs`, while the `with` line names the same agent by its workset name or short code.

The agent's screen is its transcript: everything it did, so the user can check its work or see why it is stuck. An agent has been quiet for an hour; the user opens it and sees the same error repeat; they cancel the request and set another runtime. The screen has a line on top with the agent's name, host, folder, and what it is doing; then every message it was given, every command it ran and what came back, every error, and every message it posted, newest at the bottom. A command still running shows how long it has run, counting up; a finished one shows `ok`, `error`, `cancelled`, or `ended`, and how long it took. There is no box to write in. The user talks to an agent only in its thread.

## concept Runtime
purpose: choose the model an agent thinks with and how hard it thinks.
principle: an agent starts on `opus-high`, the default; the user wants a cheaper one for a small piece of work, so they choose `set runtime…` and give it `sol-medium`.
sees: a name such as `sol-medium`, `opus-high`, or `fable-high`.

## concept Terminal
purpose: let the user type in a terminal in an agent's workset, the way the agent does.
principle: the user opens an agent's terminal from the palette; it fills the window; they type commands; `ctrl-shift-n` lets them scroll back with `j` and `k`, and `i` lets them type again.
sees: the terminal, filling the window; at the top, the agent's name, `term` and its number, and the title the program in it gives, such as `eng-q9tz · term 0 · vim`; `NORMAL` at the top right while the user reads rather than types; `terminal exited (0)` or `terminal disconnected` along the bottom when it ends; `· · · 120 lines lost · · ·` where old lines were too many to keep.
does:
- terminal, from the palette on any of the agent's screens: open its first terminal, starting one when none runs
- new terminal, from the palette: start another, numbered after the others
- type: every key goes to the terminal, `ctrl-j` and `ctrl-k` too
- paste `ctrl-shift-v`: type what is on the clipboard
- palette `ctrl-shift-;`: open the palette while typing, as from any place; see `window.Palette`
- read `ctrl-shift-n`, or `ctrl-\` then `ctrl-n`: stop typing, to scroll with `j`, `k`, `ctrl-d`, `ctrl-u`, `g g`, and `shift-g`, and to use `:`, `space`, `tab`, and `q`; `i`, `a`, or `enter` returns to typing
- scroll with the wheel: move back through old lines, or, when the program in the terminal asks for the wheel, send it the wheel

Closing a terminal's screen leaves the terminal running on the host. While it connects, the screen reads `connecting to term 0 of …`, and `q` still closes it. A terminal open on two devices is the same terminal; it takes the size of the window the user types in.

## concept Shell
purpose: give the user one line-at-a-time shell beside an agent, with its output as text they can search.
principle: the user opens the agent's shell, runs `git status`, and searches the output with `/` like any other text.
sees: at the top, the agent's name and `shell`; every command run so far, with its output under it; at the bottom, the host's prompt, such as `> `, and the line being typed.
does:
- shell, from the palette on any of the agent's screens: open the agent's shell, starting it when none runs
- write `i`: type the next command; `escape` goes back to the output
- run `enter`, while typing: run the line; the prompt reads `[sending] ` until the host has it
- interrupt `ctrl-c`, while typing: stop the command that runs
- end input `ctrl-d`, while typing on an empty line: tell the command there is no more input
- more `alt-enter`, all `alt-a`, stop `alt-q`: answer a long output that the shell holds back, which says `[pager … paused on page … — Alt-Enter more · Alt-A all · Alt-Q stop]`
- close shell, from the palette on the shell: end the shell on the host; the echo line reads `shell closed`

Closing the shell's screen leaves the shell running on the host. The agent has one shell, the same on every device. When the shell ends, the prompt reads `[shell exited 0] `; when the host drops, it reads `[shell disconnected] `, and a line that did not reach the host goes back into the line being typed.

## concept Desktop
purpose: let the user see and use a graphical app that an agent runs, such as a browser or the rho window itself.
principle: an agent runs the window it is testing on a desktop of its own; the user opens `desktop…` from the agent's screens and sees it live; they freeze a frame, circle a button that is wrong, copy the picture, and paste it into the agent's thread.
sees: the app, live, scaled to fit the window; at the top, the agent's name, `desktop`, and the desktop's name; `connecting to desktop … of …` while it opens; while drawing, a line along the bottom with `DRAW`, `u undo`, `y copy`, and `esc live`.
does:
- desktop…, from the palette on any of the agent's screens: open the agent's desktop, or, with more than one, ask `desktop:` and open the one chosen; the row shows only when the agent runs one
- every key, click, and wheel: goes to the app on the desktop
- palette `ctrl-shift-;`: open the palette without the app seeing the keys; see `window.Palette`
- draw on this frame, from the palette: hold the picture still; dragging draws red lines on it
- undo `u`, while drawing: take back the last line
- copy `y`, while drawing: put the picture, with its lines, on the clipboard; the bottom line reads `Annotation copied`
- back to live `escape` or `ctrl-g`, while drawing, or `back to live` from the palette: drop the lines and show the app live again

A desktop is also a place in `window.Palette`, as the agent's name and `desktop` and its name. Only a host reached over the network shows a desktop. From a host on this machine, the screen says that the live view needs a host reached over the network. When the window loses the keyboard, every key and button held on the desktop is let go.

## screen Agent
shows: `Agent`, its transcript
starts: `Look into what an agent did`, `Steer an agent`, `Work beside an agent`

## flow Start an agent from a message
runs: 12 a day
day: Turn ideas into work.
when: the user has an idea, or a message in a thread is work they want an agent to do
want: have an agent at work on it, in the right place, with nothing asked that the product can know
so that: the agent reads that message first and works in that thread
steps:
1. in a `chat.Channel`, the user presses `i` and writes the idea; the box reads `idea · ctrl-enter starts an agent · ctrl-shift-enter only sends`
2. the user presses `ctrl-enter`; the idea is a new thread, and an agent starts on it where the channel's agents work, on the default runtime; the echo line reads `agent … started · opus-high · …`
   - the first idea in a channel whose agents have no place yet opens the menu `where this channel's agents work`: `n` `new workset on a repository…`, `h` `here, in a directory on the host…`, or a number to join the workset of an agent already on the host; the user answers `repository (url or host path):` or `directory on the host:` when asked; the next ideas in that channel ask nothing
   - `ctrl-shift-enter` only sends, for a message that is not work
3. for a message already in a thread, the user puts the cursor on it, opens the palette and chooses `start agent, choosing where and runtime…`; it asks where and `runtime (opus-high):`, as does `send and start agent, choosing where and runtime…` for the box
system: puts the agent where the channel's newest agent works, in that agent's workset, a copy of the repository of its own that others can join; runs it on the default runtime, `opus-high`, written in the product; sends the message first when it is still in the box, and starts the agent once the host has it.
user: the words; where the agent works, once per channel; the runtime only by the rare path or `Agent.set runtime` afterwards.
cases: more than nine agents on the host, where only nine can be joined from the menu; the host is away; a reply inside a thread, where `ctrl-enter` only sends, because a reply is not an idea
reviewed: 2026-11-20 · laptop · reviewer · 4 findings

## flow Look into what an agent did
runs: 15 a day
day: Keep the work moving.
when: an agent says it is done, or has been quiet too long
want: see what it actually did
so that: the user can trust its result or see where it is stuck
steps:
1. in the agent's thread, the user puts the cursor on the `with` line and presses `enter`; the agent's screen, `Agent`, opens at its newest step
2. the user reads up through what the agent ran and what came back
3. the user goes back to the thread with `window.Screen.back`
system: shows what the agent is doing now in the top line.
user: the judgement.
cases: the agent is `retrying` after errors from its model
reviewed: 2026-11-20 · laptop · reviewer · 3 findings

## flow Steer an agent
runs: 3 a day
day: Keep the work moving.
when: an agent is stuck, going the wrong way, or on the wrong model
want: change its course without starting over
so that: its work goes on from a good point
steps:
1. the user opens the agent's screen, `Agent`
2. the user opens the palette and chooses `cancel the request`, `rewind turns`, `compact context`, `new cache key`, or `set runtime…`
3. the user goes to the agent's thread and tells it what to do instead
system: carries the change to the agent's host.
user: which change, and what to tell it.
reviewed: 2026-11-20 · laptop · reviewer · 3 findings

## flow Work beside an agent
runs: 3 a day
day: Reach anything.
when: the user wants to check or fix something in an agent's workset with their own hands
want: open its terminal, its shell, or its desktop
so that: the user sees what the agent sees and can act there
steps:
1. the user opens the agent's screen, `Agent`
2. the user opens the palette and chooses `terminal`, `new terminal`, `shell`, or `desktop…`
3. the user works there; to leave a terminal, they press `ctrl-shift-n`, then `q` or `tab`; to leave a desktop, they press `ctrl-shift-;` and choose `home`
system: starts a terminal or the shell when none runs, and keeps it running after the user leaves.
user: what to type.
cases: the agent's host is a host on this machine, where its desktop does not open; the host drops while the user types in the shell
reviewed: 2026-11-20 · laptop · reviewer · 2 findings

## rule an agent starts in its message's thread
when an `Agent` starts from a `chat.Message`, then it joins that message's `chat.Thread` and reads the message before anything else.

## flow Approve a push or a fetch
runs: 3 a day
day: Keep the work moving.
when: an agent pushes a branch to, or fetches from, a repository that needs the user's ssh login
want: see exactly where it goes and what it pushes, and say yes or no
so that: no agent uses the user's login without their word, and the agent does not wait long
steps:
1. a prompt opens by itself, over whatever is on screen; it reads the agent's name, `git asks`, what git asks, such as `Push via SSH to ssh://…: branch fix-flaky-test Approve? [shift-Y/N]`, and `allow? (y/n):`
2. the user types `y` and presses `enter` to allow
   - anything else, or `escape`, says no
   - after a minute with no answer, the push or fetch fails, and the echo line says that the approval timed out
system: uses the ssh login of the device the window runs on, so the host never holds it; asks about one push or fetch at a time; names each branch and tag a push would change.
user: the yes or the no.
cases: two agents push at once, where the second question waits for the first; a push to a repository the user does not recognise

## rule a push waits for the user
when an `Agent` pushes to or fetches from a repository through the user's ssh login, then the window asks the user first, and without a `y` the push or fetch does not happen.
