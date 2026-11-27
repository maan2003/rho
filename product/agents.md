# agents

Agents do the work behind a project's messages inside a workset. The user inspects the work in a per-workset transcript and reaches its tools (`Shell`, `Terminal`, `Desktop`) as flat peers of the `chat.Thread` and of each other.

## bet An agent is labour, not a colleague
instead of: reaching an individually named agent from each thread.
serves: Keep the work moving.
because: the user cares about the work, not the worker. The project speaks for the work, and every per-idea workset, transcript, tool, and prompt is named by `<project> · <thread subject>` (and a standing `one agent` workset simply by `<project>`) rather than opaque agent codes; short codes appear only as a quiet suffix when more than one agent runs in the same workset.
costs: the user opens a peer view to see what a particular agent ran.

## bet Trust by inspection
instead of: agents that report what they did, which the user believes.
serves: Keep the work moving.
because: the user does not edit code by hand and must still know whether a result is right; the result message carries proof, the workset transcript shows how the work was done, and the shell, terminal, and desktop let the user stand beside the work.
costs: a transcript is long, and the user reads it to find what went wrong.

## journey An agent in trouble gets back on track
goal: the user gets the work going again so the result comes.
starts when: an agent stops or fails.
steps:
1. the user opens the project's message about the stop, `deal.Clear what waits`
2. the user reads the reason, `chat.Answer a message`
   - when they need more, the user opens the workset transcript, `Look into the work`
3. the user replies with what the project should do next, `chat.Answer a message`; replying wakes an existing `stopped` or `failed` agent in its own workset and runtime (or retries a refused start using the project's latest place and runtime)
4. the user gets the result after the work carries on, `deal.Clear what waits`
done when: the user sees the project's result.
day: Keep the work moving.

## journey Work beside an agent and check it by hand
goal: the user inspects or fixes something directly in a workset's terminal, shell, or desktop and feeds back into the thread.
starts when: the user wants to verify behavior with their own eyes or hands before merging.
steps:
1. from the work's `chat.Thread` or `Transcript`, the user opens a tool (`open terminal`, `open shell`, or `open desktop`) via `window.Commands`, `Work beside an agent`
2. the user works live in `INS` (or freezes and annotates a `Desktop` frame and copies it), pressing `escape` to enter rho `NOR` (and `physical-escape` to forward literal `escape` to the guest)
3. the user flips back to the `chat.Thread` via `window.Places` (`space f` + `enter`) to reply or mark `done`
done when: the user has verified or fixed the issue and answered the thread.
day: Reach anything.

## concept Workset
purpose: group the work, its transcript, and its tools (`Shell`, `Terminal`, `Desktop`) into one inspectable unit alongside the `chat.Thread`.
principle: one or more agents work on a flaky test inside a workset; the user opens the workset transcript as a flat peer of the thread, reads the chronological log across the workset (filtering to one agent when needed), or opens its shell or desktop.
sees: its label (`<project> · <thread subject>` for per-idea worksets, or `<project>` for a standing `one agent` workset), host, folder on the host, active agents and their states (`working`, `waiting`, `failed`, `retrying`, `stopped`, or `archived`), and its full chronological transcript.
has:
- many `Agent`
- one `Shell` (experimental)
- many `Terminal`
- many `Desktop`
- many `chat.Thread`, the one it started from first
does:
- open `enter` on a project message or `Flash` in `chat.Thread` (or `open transcript` from `window.Commands`): open the workset transcript at the step that wrote the message or flash (or the last error / newest step when opened by command); once visited, the open transcript stays in `window.Places` MRU recents
- filter transcript, from `window.Commands` on the transcript: narrow the workset transcript to one `Agent`, or clear the filter to show the whole workset
- open thread, from `window.Commands`: open the `chat.Thread` it started from
todo:
- workset lifecycle and cleanup (archiving finished clones/tools without a half-baked `finish work` command) · 2026-11-27 · design clean workset archival/cleanup later; removed ad-hoc `finish work` command
rejected:
- an ad-hoc `finish work` command · 2026-11-27 · collided with standing `one agent` worksets and felt awkward; revisit workset lifecycle properly under `todo:`

## concept Agent
purpose: do a piece of the user's work inside a `Workset` on a host.
principle: an agent works from the user's message about a flaky test; if it stalls or uses the wrong model, the user steers it from `window.Commands` anywhere (using natural verb-noun names or weighted aliases like `kill` or `model`, and overriding the context target inline when needed).
sees: its work label (`<project> · <thread subject>` for per-idea worksets, `<project>` for standing `one agent` worksets, with its short code such as `eng-q9tz` as a quiet suffix only when >1 agent runs in the workset), runtime, and state (`working`, `waiting`, `failed`, `retrying`, `stopped`, or `archived`). There is no user-facing `idle` agent state or idle notice.
does:
- rewind agent, from `window.Commands` anywhere: uses the agent in context by default (overridable inline) or prompts for the agent; takes back its last turn and waits for the user; the echo line reads `rewound one turn: tide · Flaky clock test · waiting for you`
- rewind agent to here, from `window.Commands` on a `Transcript` step: rewinds the agent back to the selected step (taking back every turn after it)
- compact agent, from `window.Commands` anywhere: shortens what the agent holds in mind; the echo reads `compacted: tide · Flaky clock test`
- reset agent cache, from `window.Commands` anywhere: starts the agent's model cache afresh; the echo reads `reset cache: tide · Flaky clock test`
- stop agent, from `window.Commands` anywhere (also matched by weighted aliases such as `kill`; supports inline target override like `:stop agent clock`): ends its work and keeps the transcript; the thread says `stopped by you`, with no new message; a later reply in the thread wakes it in its own workset and runtime
- set agent runtime, from `window.Commands` anywhere (supports inline arg like `:model sol-medium`, or asks `runtime (current):` with the host's other models and efforts of its kind): switches the agent's runtime; a stopped agent that has never sent a request can also change to another kind; the echo reads `runtime: tide · Flaky clock test · sol-low (was sol-medium)`
- cancel agent request, from `window.Commands` anywhere (also matched by weighted aliases such as `kill` / `abort`): stops what it is asking its model now; the echo reads `cancelled: tide · Flaky clock test` or `not cancelled: tide · Flaky clock test · it was not asking its model`

While the agent's host is `away`, each of these echoes `the agent's host is away`; when the host is not attached at all, it echoes `the agent's host is not attached`.
todo:
- meta-agent / supervisor that detects silent or dead agents (e.g. ending a turn with flashes only and no `chat.Message`) · 2026-11-27 · intentionally deferred; keep turn completion unblocked for now and revisit with a dedicated supervisor design later

## concept Runtime
purpose: choose the model an agent thinks with and how hard it thinks.
principle: an agent starts on its project's runtime, `opus-high` until the project says otherwise, or on the one the idea's `on:` line named; the user wants a cheaper one for a small piece of work already running, so they type `:model sol-medium` in `window.Commands`.
sees: a name such as `sol-medium`, `opus-high`, or `fable-high`.

## concept Shell
status: experimental.
purpose: give the user one line-at-a-time shell beside a workset, with its output as text they can search.
principle: the user opens the workset's shell from `window.Commands`, runs `git status`, and searches the output with `/` like any other text.
sees: at the top, `<workset label> · shell`; every command run so far, with its output under it; at the bottom, the host's prompt, such as `> `, and the line being typed.
does:
- open shell, from `window.Commands` anywhere: opens the workset's shell (picking the workset in context or prompting by workset label), starting one when none runs; once visited, it appears in `window.Places` MRU recents
- write `i`: type the next command; `escape` returns to `NOR` (output and window keys); `physical-escape` sends a literal `escape` while typing
- recall `up` or `ctrl-p`, `down` or `ctrl-n` while typing: put the previous or next command on the input line; `enter` on an earlier command recalls that command without running it
- run `enter`, while typing: run the line; the prompt reads `[sending] ` until the host has it
- interrupt `ctrl-c`, while typing: stop the command that runs
- end input `ctrl-d`, while typing on an empty line: tell the command there is no more input
- more `alt-enter`, all `alt-a`, stop `alt-q`: answer a long output that the shell holds back, which says `[pager … paused on page … — Alt-Enter more · Alt-A all · Alt-Q stop]`
- close shell, from `window.Commands`: end the shell on the host; the echo line reads `shell closed`

While a command runs, the prompt reads `running · 4s`, counting up. Closing the shell's screen leaves the shell running on the host. When the shell ends, the prompt reads `[shell exited 0] `; when the host drops, it reads `[shell disconnected] `, and a line that did not reach the host goes back into the line being typed.

## concept Terminal
purpose: let the user type in a full terminal in a workset when a line-at-a-time shell is not enough.
principle: the user opens a workset's terminal from `window.Commands`; it fills the window; they type commands; `escape` returns to rho `NOR` for scrollback and window navigation, `physical-escape` sends literal `escape` to the program inside, and `i` returns to typing.
sees: the terminal, filling the window; at the top, `<workset label> · term <n>` and the title the program in it gives, such as `tide · Flaky clock test · term 0 · vim`; `INS` while typing and `NOR` while reading; `terminal exited (0)` or `terminal disconnected` along the bottom when it ends; `· · · 120 lines lost · · ·` where old lines were too many to keep.
does:
- open terminal, from `window.Commands` anywhere: opens a workset's running terminal (prompting when more than one runs, or starting one when none runs); once visited, it appears in `window.Places` MRU recents (never cluttering global `window.Places` search)
- new terminal, from `window.Commands` anywhere: start another terminal, numbered after the others
- type (`INS`): keys go to the terminal, except `escape` (enters rho `NOR`), `physical-escape` (forwards literal `escape` to the terminal), `ctrl-shift-v` (paste), `ctrl-shift-;` (opens `window.Commands`), `ctrl-alt-m` (idea draft), and `space-j` / `space-shift-j` (deal)
- read (`NOR`): standard `window.Screen` keys (`space` leader for `f` places / `u` undo, `space-j` / `ctrl-j`, `tab`, `q`, `:`) plus scrollback (`j`, `k`, `ctrl-d`, `ctrl-u`, `gg`, `G`), search (`/`, `?`, `n`, `N`), and copy (`y`); `i` or `a` returns to typing
- scroll with the wheel: move back through old lines, or send the wheel to the program when it asks for it

Closing a terminal's screen leaves it running on the host. While it connects, the screen reads `connecting to term 0 of …`, and `q` still closes it. A terminal open on two devices is the same terminal; it takes the size of the window the user types in.

## concept Desktop
purpose: let the user see and use a graphical app that a workset runs, such as a browser or the rho window itself.
principle: an agent runs the window it is testing on a desktop of its own; the user opens `open desktop` from `window.Commands` and sees it live; they press `escape` to enter rho `NOR` (to leave, deal, or open `window.Places` / `window.Commands`), or `physical-escape` to send `escape` to the app; they freeze a frame, circle a button that is wrong, copy the picture, and paste it into the work's thread.
sees: the app, live, scaled to fit the window; at the top, `<workset label> · desktop` and the desktop's name, with `INS` while live and `NOR` when escaped; `connecting to desktop … of …` while it opens; while drawing, a line along the bottom with `DRAW`, `u undo`, `y copy`, and `esc live`.
does:
- open desktop, from `window.Commands` anywhere: open the workset's desktop, or prompt `desktop:` when more than one runs; once visited, it appears in `window.Places` MRU recents (never cluttering global `window.Places` search)
- live input (`INS`): keys (including `space`), clicks, and wheel go to the remote app without leaking reserved chords; `escape` enters rho `NOR`; `physical-escape` sends literal `escape` to the remote app; `ctrl-shift-;` opens `window.Commands`; `space-j` / `space-shift-j` deal directly
- normal (`NOR`): standard `window.Screen` keys (`space` leader for `f` places / `u` undo, `space-j` / `ctrl-j`, `tab`, `q`, `:`); `i` or clicking the app returns to live input
- draw on desktop, from `window.Commands`: hold the picture still; dragging draws red lines on it
- undo `u`, while drawing: take back the last line
- copy `y`, while drawing: put the picture, with its lines, on the clipboard; the bottom line reads `Annotation copied`
- back to live `escape` or `ctrl-g`, while drawing, or `back to live` from `window.Commands`: drop the lines and show the app live again

Only a host reached over the network shows a desktop. From a host on this machine, the screen says that the live view needs a host reached over the network. When the window loses the keyboard, every key and button held on the desktop is let go.

## screen Transcript
answers: what happened in this workset, and on what brief?
shows:
- in the borderless Left Ear of `window.Status line`, `<workset label>` (and active agent filter if >1 agent), project, and runtime/model (such as `opus-high` — moved here from `chat.Thread`), plus host/folder/activity and any errors since its last successful step inside the `128 ch` transcript sheet
- inside the editor buffer itself (never truncated or pinned outside it): the full brief once, then the chronological transcript — messages with `design.Message header` (`you · 12:22`), commands showing their whole wrapped line, output open, and repeated errors folded (`×7`); nothing the work was given is shortened, and everything is searchable and copyable with Helix motions
states:
- empty: the brief and workset state while no steps have run yet
- loading: connecting to host / loading transcript without discarding the screen
- partial: available events while syncing
- error: host unreachable or workset missing
- full: full brief and chronological transcript (optionally filtered to one `Agent`)
keys:
- `]e`, `[e`: the next or previous error
- `space`: silent `window.Screen` prefix (`space f` places, `space u` undo, plus `deal.Verdict` chords on the workset's waiting thread when unambiguous); the transcript stays open
- `space-j` (`ctrl-j`), `tab`, `q`, `:`: `window.Screen`
starts: `Steer an agent`, `Work beside an agent`
captures: [message's writing step on the laptop](design/laptop-agent.png) and [on the phone](design/phone-agent.png).
rejected:
- pinning the brief in a fixed 4-line clamped header outside the editor · 2026-11-26 · truncates multi-paragraph briefs, breaks search and yank, and wastes viewport height
- requiring navigation through an `Agent` screen before reaching `Shell`, `Terminal`, or `Desktop` · 2026-11-26 · all workset views are flat peers reachable directly from `chat.Thread` and `window.Commands`

## flow Look into the work
runs: 15 a day
day: Keep the work moving.
when: the user has a result or a report of trouble and wants to see how the work was done
want: see what actually happened without making agent activity part of the thread
so that: the user can trust the result or understand where the work is stuck
steps:
1. the user opens the work's `chat.Thread`
2. the user presses `enter` on the message or `Flash` (or runs `open transcript` from `window.Commands`) to open the `Workset` transcript at the step that wrote it
   - if multiple agents ran in the workset and the user wants only one, they run `filter transcript` from `window.Commands`
3. the user reads what ran and what came back (and can run `rewind agent to here` on any step if needed)
4. the user flips back to the thread via `window.Places` (`space` then `f` + `enter`) or `q`
system: keeps the transcript with the work it explains, and keeps it open as a flat peer of the thread.
user: the judgement.
cases: a stopped agent; more than one agent worked in the workset; opening from a `Flash`.

## flow Steer an agent
runs: 3 a day
day: Keep the work moving.
when: an agent is stuck, going the wrong way, or on the wrong model
want: change its course without starting over
so that: its work goes on from a good point
steps:
1. from anywhere (`chat.Thread`, `Transcript`, `desk.Desk`), the user opens `window.Commands` (`:`) and chooses `cancel agent request`, `stop agent`, `rewind agent`, `rewind agent to here`, `compact agent`, `reset agent cache`, or `set agent runtime` (optionally with an inline target or argument such as `:model sol-medium` or `:stop agent clock`)
   - when one agent is unambiguous from context, the row shows it by default (overridable inline); otherwise a `window.Prompt` asks which agent (by workset label)
2. if the work needs new instructions, the user writes to the project in `chat.Thread`, `chat.Answer a message`
system: carries the change to the agent's host.
user: which change, and what to tell it.

## flow Work beside an agent
runs: 3 a day
day: Reach anything.
when: the user wants to check or fix something in a workset with their own hands
want: open its `Shell`, `Terminal`, or `Desktop` directly
so that: the user sees what the agent sees and can act there
steps:
1. from `chat.Thread`, `Transcript`, or anywhere via `window.Commands`, the user chooses `open shell`, `open terminal`, `new terminal`, or `open desktop` (or flips back to a visited session in `window.Places` MRU recents)
2. the user works there; pressing `escape` enters rho `NOR` (so `space f`, `tab`, `q`, `:` work identically everywhere), `physical-escape` sends a literal `escape` to the guest, and `space-j` deals directly
system: starts a shell or terminal when none runs, keeps it running after the user leaves, and puts visited sessions into `window.Places` MRU recents.
user: what to type.
cases: more than one running terminal; the host is on this machine, where desktop does not open; the host drops while typing in the shell.

## rule an idea starts the work in its thread
when the user sends a `chat.Message` as a new idea to a `chat.Project`, then the work starts in the `chat.Thread` for that idea and the agent reads the idea first. In `one` (`one agent`) mode it is the project's standing agent, started when none runs; in `new` (`per idea`) mode it is a fresh agent in its own workset, as `chat.Project.receive an idea`.

## flow Approve a push or a fetch
runs: 3 a day
day: Keep the work moving.
when: an agent pushes to `main` or `master`, or pushes to / fetches from a repository that needs the user's ssh login
want: see exactly which work asks, where it goes, and what it pushes, right in its thread, and say yes or no when ready — without being interrupted mid-typing or failing after a minute away
so that: no agent pushes to a protected branch or uses the user's ssh login without their word, denying can carry instructions, and background work does not time out while the user is busy or away
steps:
1. the host stages the exact push or fetch intent (target repository, ref changes including force-push dropped count, up to 50 commits, and bounded diff) and surfaces it right at the top of its originating user `chat.Thread` (a child agent's staged push bubbles up to the parent user thread so it never strands in an agent-to-agent thread; a thread the user explicitly muted stays silenced). Lights only the Top-Right Global Status Bar lamp — never popping a modal over what the user is typing
2. when the user deals or opens the thread, they inspect the repository, ref changes, commits, and diff in full thread context and give `approve` (`space` then `a`, refused if the host is `away`), `deny` (`space` then `x`, opening `:deny push ` prefilled with any current draft text so `enter` confirms), or `snooze` (`space` then `s` → `:snooze thread `); the thread never offers `done` or `mute` while a push/fetch awaits, and `approve`/`deny` are host-visible acting decisions (not undoable with `space u` once executed). Agents may also withdraw their own staged push so a plain reply can resolve it
system: keeps the user uninterrupted; holds the staged push asynchronously with no 60-second timeout, also while no window is open; executes using the user's ssh login once approved only if Git sets the exact refs and commits shown; takes the pending push away when the agent's git goes away, such as when the agent is stopped. For non-`main`/non-`master` branches on GitHub, a host configured with `GITHUB_TOKEN` may push directly through `octo`, which refuses `main`, `master`, and tags.
user: the approve or deny decision (plus optional draft instructions on deny).
cases: two agents request protected pushes at once (two waiting threads in the deal); a push requested while the user is typing in a terminal or while the laptop is shut; denying with "push a branch and open a PR instead"; a push to a repository the user does not recognise.
rejected:
- a synchronous 60-second pop-over prompt over whatever screen is active · 2026-11-26 · stole keypresses, broke `One place, in focus` (`The lamp is the only pull allowed`), and always timed out while away
- a standalone push-approval card screen separate from its `chat.Thread`, with `space d` as deny · 2026-11-26 · `space d` collided with 50×/day `done` muscle memory, separated the push from its thread context, and couldn't attach reply instructions to a denial

## rule a push waits for the user
when an `Agent` pushes to `main` or `master`, or pushes/fetches through the user's ssh login, then it parks asynchronously at the top of its originating `chat.Thread` until the user explicitly approves (`space a`) or denies (`space x`) it; without approval the push or fetch does not happen. An approved push goes only if Git sets exactly the refs, to exactly the commits, that the thread showed; otherwise it fails and the agent asks again. Feature-branch pushes to GitHub may run unprompted through a host-held `GITHUB_TOKEN` via `octo`, which enforces that only non-`main`/non-`master` branches may be pushed.

## rule a refused start leaves a message
when a start cannot use the `chat.Project`'s place before it creates an `Agent`, then the `chat.Thread` keeps a `failed` stop that names the place and the host and says `reply to retry, or change where it works with set project place`. A later start shows its own progress without erasing the stop from history.
rejected:
- telling the user to run `start agent here` · 2026-11-21 · no command has that name, and the place belongs to the project

## rule a stuck agent asks
when an `Agent` has been retrying its model for three minutes, then its thread says `stuck · can't reach its model` and the age of the retry, and the project sends one stop message. When the model answers again, the project edits that stop and it no longer deals.

## rule rewinding takes back its messages
when `rewind agent` or `rewind agent to here` takes back a turn that wrote message, then the withdrawn message is still readable as `taken back` but no longer deals. Message since changed by another turn is not withdrawn.
