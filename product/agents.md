# agents

The agents do the work behind the project's mail. The user goes one level down from the work to see an agent's transcript, terminals, shell, or desktops.

## bet An agent is labour, not a colleague
instead of: reaching an individually named agent from each thread.
serves: Keep the work moving.
because: the user cares about the work, not the worker. The project speaks for the work and an agent is reached only when the user needs to look into it.
costs: the user goes one level down to see what a particular agent did.

## bet Trust by inspection
status: the transcript, terminal, shell and desktop are built; the result does not yet carry proof.
instead of: agents that report what they did, which the user believes.
serves: Keep the work moving.
because: the user does not edit code by hand and must still know whether a result is right; the result mail carries proof, the transcript shows how the work was done, and the terminal, shell, and desktop let the user stand beside the agent.
costs: a transcript is long, and the user reads it to find what went wrong.

## journey An agent in trouble gets back on track
goal: the user gets the work going again so the result comes.
starts when: an agent stops or goes quiet.
steps:
1. the user opens the project's mail about the stop, `deal.Clear what waits`
2. the user reads the reason, `mail.Answer a mail`
   - when they need more, the user goes into the transcript, `Look into the work`
3. the user replies with what the project should do next, `mail.Answer a mail`
4. the user gets the result after the work carries on, `deal.Clear what waits`
done when: the user sees the project's result.
day: Keep the work moving.

## concept Agent
status: the thread keeps replies to an agent that cannot launch, but they do not wake it. The user changes its runtime or starts another agent to carry on.
purpose: do a piece of the user's work on a host, and let the user inspect what was done if needed.
principle: an agent works from the user's mail about a flaky test. When the result arrives from the project, the user goes from the thread to the transcript to see what the agent ran.
sees: its name, which is its workset's name with its short code, such as `manifold-release-and-bridge (eng-kjhi)`, or the short code alone, such as `eng-q9tz`; its host; its folder on the host, such as `/src/rho`; what it is doing: `working`, `waiting`, `idle`, `failed`, `retrying`, or `archived`.
has:
- many `Terminal`
- one `Shell`
- many `Desktop`
- many `mail.Thread`, the one it started from first
does:
- open `enter` or a tap on a mail header: from the `mail.Thread` one level down, at the step that wrote the mail as it now reads; when opened by name, a retrying or failed agent opens on its last error, otherwise on its newest step
- open its thread, from the palette as `open its thread`: open the thread it started from
- rewind turns, from the palette: take back its last turn and wait for the user; the palette has no way to give a count, so it is always one; the echo line reads `rewound one turn: eng-rb89 · waiting for you`
- compact context, from the palette: shorten what it holds in mind; the echo reads `compacted: eng-rb89`
- new cache key, from the palette: make it start its model's cache afresh; the echo reads `new cache key: eng-rb89`
- stop agent, from the palette: end its work and keep the transcript; the thread says `stopped by you`, with no new mail.
- set runtime, from the palette: ask `runtime (current):`, offering the host's other models and efforts of its kind, one word each; a stopped agent that has never sent a request can also change to another kind. To change kinds after the agent has sent a request, start another agent. The echo reads `runtime: eng-ka35 · sol-low (was sol-medium)`.
- cancel the request, from the palette: stop what it is asking its model now; the echo reads `cancelled: eng-rb89` or `not cancelled: eng-rb89 · it was not asking its model` when there was no request

While the agent's host is away, each of these echoes `the agent's host is not attached`.

The agent's screen is its transcript: everything it did, so the user can check its work or see why it is stuck. Each command line ends with `ok` or `error` and how long it ran; the same error again counts on one line, `×7`; a cancel reads `you cancelled the request`. An agent has been quiet for an hour; the user opens it and sees the same error repeat; they cancel the request and set another runtime. The screen has a line on top with the agent's name, host, folder, and what it is doing; then every message it was given, every command it ran and what came back, every error, and every message it posted, newest at the bottom. A command still running shows how long it has run, counting up; a finished one shows `ok`, `error`, `cancelled`, or `ended`, and how long it took. There is no box to write in. The user writes to the project in `mail.Thread`; the transcript is for looking, not for replying.

## concept Runtime
purpose: choose the model an agent thinks with and how hard it thinks.
principle: an agent starts on `opus-high`, the default; the user wants a cheaper one for a small piece of work, so they choose `set runtime…` and give it `sol-medium`.
sees: a name such as `sol-medium`, `opus-high`, or `fable-high`.

## concept Terminal
purpose: let the user type in a terminal in an agent's workset, the way the agent does.
principle: the user opens an agent's terminal from the palette; it fills the window; they type commands; `ctrl-shift-n` lets them scroll back with `j` and `k`, and `i` lets them type again.
sees: the terminal, filling the window; at the top, the agent's name, `term` and its number, and the title the program in it gives, such as `eng-q9tz · term 0 · vim`; `NOR` at the top right while the user reads rather than types; `terminal exited (0)` or `terminal disconnected` along the bottom when it ends; `· · · 120 lines lost · · ·` where old lines were too many to keep.
does:
- terminal, from the palette on any of the agent's screens: open its first terminal, starting one when none runs
- new terminal, from the palette: start another, numbered after the others
- type: every key goes to the terminal, `ctrl-j` and `ctrl-k` too
- paste `ctrl-shift-v`: type what is on the clipboard
- palette `ctrl-shift-;`: open the palette while typing, as from any place; see `window.Palette`
- read `ctrl-shift-n`, or `ctrl-\` then `ctrl-n`: stop typing, to scroll with `j`, `k`, `ctrl-d`, `ctrl-u`, `g g`, and `shift-g`, and to use `:`, `space`, `tab`, and `q`; `/`, `?`, `n`, `N` search and `y` copies as in other text; only `i` or `a` returns to typing
- scroll with the wheel: move back through old lines, or, when the program in the terminal asks for the wheel, send it the wheel

On the phone, the terminal bar has `esc`, `ctrl`, `tab`, the four arrows and `read` while typing; `ctrl` modifies the next key. While reading the bar is `back`, `desk`, `menu`, `search`, `type`. Closing a terminal's screen leaves the terminal running on the host. While it connects, the screen reads `connecting to term 0 of …`, and `q` still closes it. A terminal open on two devices is the same terminal; it takes the size of the window the user types in.

## concept Shell
purpose: give the user one line-at-a-time shell beside an agent, with its output as text they can search.
principle: the user opens the agent's shell, runs `git status`, and searches the output with `/` like any other text.
sees: at the top, the agent's name and `shell`; every command run so far, with its output under it; at the bottom, the host's prompt, such as `> `, and the line being typed.
does:
- shell, from the palette on any of the agent's screens: open the agent's shell, starting it when none runs
- write `i`: type the next command; `escape` goes back to the output
- recall `up` or `ctrl-p`, `down` or `ctrl-n` while typing: put the previous or next command on the input line; `enter` on an earlier command recalls that command without running it
- run `enter`, while typing: run the line; the prompt reads `[sending] ` until the host has it
- interrupt `ctrl-c`, while typing: stop the command that runs
- end input `ctrl-d`, while typing on an empty line: tell the command there is no more input
- more `alt-enter`, all `alt-a`, stop `alt-q`: answer a long output that the shell holds back, which says `[pager … paused on page … — Alt-Enter more · Alt-A all · Alt-Q stop]`
- close shell, from the palette on the shell: end the shell on the host; the echo line reads `shell closed`

While a command runs, the prompt reads `running · 4s`, counting up. On the phone, tap the prompt or `write` to type, `↑` to recall, and `run` or `cancel` to stop typing. `stop` interrupts the running command, like `ctrl-c`. Closing the shell's screen leaves the shell running on the host. The agent has one shell, the same on every device. When the shell ends, the prompt reads `[shell exited 0] `; when the host drops, it reads `[shell disconnected] `, and a line that did not reach the host goes back into the line being typed.

## concept Desktop
purpose: let the user see and use a graphical app that an agent runs, such as a browser or the rho window itself.
principle: an agent runs the window it is testing on a desktop of its own; the user opens `desktop…` from the agent's screens and sees it live; they freeze a frame, circle a button that is wrong, copy the picture, and paste it into the work's thread.
sees: the app, live, scaled to fit the window; at the top, the agent's name, `desktop`, and the desktop's name; `connecting to desktop … of …` while it opens; while drawing, a line along the bottom with `DRAW`, `u undo`, `y copy`, and `esc live`.
does:
- desktop…, from the palette on any of the agent's screens: open the agent's desktop, or, with more than one, ask `desktop:` and open the one chosen; the row shows only when the agent runs one
- verdict `space`: open `deal.Verdict` on the agent's thread without leaving the desktop
- other keys, clicks, and wheel: go to the app on the desktop
- palette `ctrl-shift-;`: open the palette without the app seeing the keys; see `window.Palette`
- draw on this frame, from the palette: hold the picture still; dragging draws red lines on it
- undo `u`, while drawing: take back the last line
- copy `y`, while drawing: put the picture, with its lines, on the clipboard; the bottom line reads `Annotation copied`
- back to live `escape` or `ctrl-g`, while drawing, or `back to live` from the palette: drop the lines and show the app live again

A desktop of a known agent on an attached host is also a place in `window.Palette`, as the agent's name and `desktop` and its name. Only a host reached over the network shows a desktop. From a host on this machine, the screen says that the live view needs a host reached over the network. When the window loses the keyboard, every key and button held on the desktop is let go.

## screen Agent
answers: what went wrong, on what brief?
shows:
- in the `window.Status line`, the agent, its host, folder and runtime, what it is doing, and the errors since its last successful step. On a phone the folder and host give way before the agent's name.
- the brief once, then the transcript in order: mail uses `design.Mail header` (`you · 12:22`) in reading text, commands use the user's words, and output is folded except on errors
keys:
- `]e`, `[e`: the next or previous error; on the phone, `error` takes the `desk` slot when there are errors
- `enter` or a tap on a command: open or fold its output
- `space` then `u`: undo the last verdict without leaving the transcript
- `space`: `deal.Verdict` on the agent's thread; the transcript stays open
- `ctrl-k`: `window.Screen.back`
starts: `Steer an agent`, `Work beside an agent`

Built renders: [mail's writing step on the laptop](design/laptop-agent.png) and [on the phone](design/phone-agent.png).

mock:
```text
lab › eng-ka35 · clock · sol-low   retrying · 12 errors           NOR
brief
Fix the clock test that fails one run in five.
──────────────────────────────────────────────────────────────────
you · 12:22
Use a separate clock per test. Keep the change small.
  set status "Waiting for clarification" · ok 12ms …
  wrote to you: "Could you clarify which behavior you want me to implement?"
you · 12:22
Also check the retry path.
  can't reach its model sol-low · retrying ×12 · since 12:22
```

mock, phone:
```text
eng-ka35 · retrying · 12 errors            NOR
brief
Fix the clock test that fails one run in five.
──────────────────────────────────────────────
you · 12:22
Also check the retry path.
can't reach its model sol-low · retrying ×12
· since 12:22

      back            error           menu
```

## flow Look into the work
runs: 15 a day
day: Keep the work moving.
when: the user has a result or a report of trouble and wants to see how the work was done
want: see what actually happened without making agent activity part of the thread
so that: the user can trust the result or understand where the work is stuck
steps:
1. the user opens the work's `mail.Thread`
2. the user presses `enter` on the mail or taps its header to go one level down to the `Agent` transcript
   - away from an agent's mail, `look into the work` asks which agent, newest first and with its runtime
3. the user reads what the agent ran and what came back
4. the user goes back to the thread with `window.Screen.back`
system: keeps the transcript with the work it explains.
user: the judgement.
cases: a stopped agent; more than one agent worked on the piece.
reviewed: 2026-11-20 · laptop · reviewer · 3 findings

The historical review was of Look into what an agent did.

## flow Steer an agent
runs: 3 a day
day: Keep the work moving.
when: an agent is stuck, going the wrong way, or on the wrong model
want: change its course without starting over
so that: its work goes on from a good point
steps:
1. the user opens the agent's screen, `Agent`
2. the user opens the palette and chooses `cancel the request`, `rewind turns`, `compact context`, `new cache key`, or `set runtime…`
3. if the work needs new instructions, the user returns to the thread and writes to the project, `mail.Answer a mail`
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
3. the user works there; to leave a terminal, they press `ctrl-shift-n`, then `q` or `tab`; to leave a desktop, they press `ctrl-shift-;` and open the desk, `desk.Desk`
system: starts a terminal or the shell when none runs, and keeps it running after the user leaves.
user: what to type.
cases: the agent's host is a host on this machine, where its desktop does not open; the host drops while the user types in the shell
reviewed: 2026-11-20 · laptop · reviewer · 2 findings

## rule an idea starts the work in its thread
when the user sends a `mail.Mail` as a new idea to a `mail.Project`, then the agent starts in the `mail.Thread` for that work and reads the idea first.

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

## rule a refused start leaves mail
when `start agent here` cannot use the chosen host directory before it creates an `Agent`, then the `mail.Thread` keeps a `failed` stop with the directory and host and tells the user how to choose again. A later start shows its own progress without erasing the stop from history.

## rule a stuck agent asks
when an `Agent` has been retrying its model for three minutes, then its thread says `stuck · can't reach its model` and the age of the retry, and the project sends one stop mail. When the model answers again, the project edits that stop and it no longer deals.

## rule rewinding takes back its mail
when `rewind turns` takes back a turn that wrote mail, then the withdrawn mail is still readable as `taken back` but no longer deals. Mail since changed by another turn is not withdrawn.
