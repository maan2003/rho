# agents

The agents do the work. The user starts one from a message, talks to it in its thread, and looks into what it does: its transcript, its terminals, its shell, and its desktops. Agents have no list and no screen of their own to visit; each is reached from the thread it works in, or by name.

## concept Agent
purpose: do a piece of the user's work on a host, in a thread the user can read and answer.
principle: the user starts an agent from the message `fix the flaky test`; the agent joins that thread, reads the message first, works in its workset, and posts there; when it asks, the thread becomes a card.
sees: its name, which is its workset's name with its short code, such as `manifold-release-and-bridge (eng-kjhi)`, or the short code alone, such as `eng-q9tz`; its host; its folder on the host, such as `/src/rho`; what it is doing: `working`, `waiting`, `idle`, `failed`, `retrying`, or `archived`.
has:
- one `Transcript`
- many `Terminal`
- one `Shell`
- many `Desktop`
- many `chat.Thread`, the one it started from first
does:
- open: from the `with` line of a thread, or by name with `window.Find`
- open its thread, from the palette as `open its thread`: open the thread it started from
- rewind turns, from the palette, with an optional count: take back its last turns, one unless a count is typed
- compact context, from the palette: shorten what it holds in mind
- new cache key, from the palette: make it start its model's cache afresh
- set runtime, from the palette: ask `runtime (current):` and switch the model it runs on
- cancel the request, from the palette: stop what it is asking its model now

In a thread, a message from an agent is signed with its long code, such as `q9tz45g2bcvs`, while the `with` line names the same agent by its workset name or short code.

## concept Transcript
purpose: show everything an agent did, so the user can check its work or see why it is stuck.
principle: an agent has been quiet for an hour; the user opens its transcript and sees the same error repeat; they cancel the request and set another runtime.
sees: a line on top with the agent's name, host, folder, and what it is doing; then every message it was given, every command it ran and what came back, every error, and every message it posted, newest at the bottom.

There is no box to write in. The user talks to an agent only in its thread.

## concept Workset
purpose: give an agent a place on a host to work, with its own copy of a repository.
principle: the user starts an agent with `new workset on a repository…` and gives the repository; the agent works in a copy of its own; a second agent can join that workset to work beside it.
sees: its name, which is also the name of the agents in it.

A new agent gets a new workset on a repository, works in a folder on the host as it is, or joins the workset of an agent already there.

## concept Runtime
purpose: choose the model an agent thinks with and how hard it thinks.
principle: the user starts an agent and takes `sol-medium`; it struggles, so they choose `set runtime…` and give it `opus-high`.
sees: a name such as `sol-medium`, `opus-high`, or `fable-high`.

## concept Terminal
purpose: let the user type in a terminal in an agent's workset, the way the agent does.
principle: the user opens an agent's terminal from the palette; it fills the window; they type commands; `ctrl-shift-n` lets them scroll back with `j` and `k`, and `i` lets them type again.
does:
- terminal, from the palette on any of the agent's screens: open its first terminal, starting one when none runs
- new terminal, from the palette: start another
- palette `ctrl-shift-;`: open the palette while typing
- read `ctrl-shift-n`: stop typing, to scroll and use `:`, `space`, `tab`, and `q`; `i`, `a`, or `enter` returns to typing

Closing a terminal's screen leaves the terminal running on the host.

## concept Shell
purpose: give the user one line-at-a-time shell beside an agent, with its output as text they can search.
principle: the user opens the agent's shell, runs `git status`, and searches the output with `/` like any other text.
does:
- shell, from the palette on any of the agent's screens: open the agent's shell, starting it when none runs
- run `enter`: run the line
- interrupt `ctrl-c`, end input `ctrl-d`
- more `alt-enter`, all `alt-a`, quit `alt-q`: page through long output
- close shell, from the palette on the shell: end the shell on the host

Closing the shell's screen leaves the shell running on the host.

## concept Desktop
purpose: let the user see and use a graphical app that an agent runs, such as a browser or the rho window itself.
principle: an agent runs the window it is testing on a desktop of its own; the user opens `desktop…` from the agent's screens, picks it, and sees it live.
sees: the app, live; `connecting to desktop …` while it opens.
does:
- desktop…, from the palette on any of the agent's screens: choose one of the agent's desktops and open it; the row shows only when it has one
- every key: goes to the app on the desktop
- palette `ctrl-shift-;`: open the palette without the app seeing the keys
- draw on this frame, from the palette: hold the picture still and draw on it; `back to live` returns

## screen Transcript
shows: `Agent`, `Transcript`
starts: `Look into what an agent did`, `Steer an agent`

## job Start an agent from a message
when: a message in a thread, mine or not yet sent, is work I want an agent to do
want: start an agent on it, in the right place, on the right model
so that: the agent reads that message first and works in that thread
flow:
1. put the cursor on the message, or write the message in the box
2. open the palette and choose `start agent here`, or `send and start agent` for the message in the box
3. pick where in the menu: `n` a new workset on a repository, `h` a folder on the host, or a number to join an agent already there
4. answer the prompt for the repository or the folder, when asked
5. answer `runtime (sol-medium):`, or press `enter` for that runtime
6. the echo line reads `agent … started`, and the agent joins the thread
system: lists the thread's own agents first among those to join, then the host's other agents; sends the message first when it is still in the box, and starts the agent once the host has it.
user: where the agent works, and its runtime.
cases: more than nine agents on the host, where only nine can be joined from the menu; the host is away

## job Look into what an agent did
when: an agent says it is done, or has been quiet too long
want: see what it actually did
so that: I can trust its result or see where it is stuck
flow:
1. in its thread, put the cursor on the `with` line and press `enter`; the `Transcript` opens at its newest step
2. read up through what it ran and what came back
3. `window.Screen.back` to the thread
system: shows what the agent is doing now in the top line.
user: the judgement.
cases: the agent is `retrying` after errors from its model

## job Steer an agent
when: an agent is stuck, going the wrong way, or on the wrong model
want: change its course without starting over
so that: its work goes on from a good point
flow:
1. open its `Transcript`
2. open the palette and choose `cancel the request`, `rewind turns` with a count, `compact context`, `new cache key`, or `set runtime…`
3. go to its thread and tell it what to do instead
system: carries the change to the agent's host.
user: which change, and how many turns.

## job Work beside an agent
when: I want to check or fix something in an agent's workset with my own hands
want: open its terminal, its shell, or its desktop
so that: I see what it sees and can act there
flow:
1. open the agent's `Transcript`
2. open the palette and choose `terminal`, `new terminal`, `shell`, or `desktop…`
3. work there; to leave a terminal, press `ctrl-shift-n`, then `q` or `tab`
system: starts a terminal or the shell when none runs, and keeps it running after the user leaves.
user: what to type.

## rule an agent starts in its message's thread
when an `Agent` starts from a `chat.Message`, then it joins that message's `chat.Thread` and reads the message before anything else.
