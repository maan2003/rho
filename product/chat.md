# chat

Each piece of work is a thread, with a subject, optional inline tags and actors, and the messages so far. The user starts a new thread by typing directly on the blank `desk.Desk` canvas (or pressing `ctrl-alt-m` from anywhere) using inline `:foo` and `:foo <value>` tokens; while the work runs, the thread's latest message says what is happening now.

## bet A message is the record, a flash is the now
instead of: a chat stream where everything an agent says has the same weight, or mail where every word is kept.
serves: Keep the work moving.
because: one thread keeps one piece of work together. Its subject identifies the work, and the thread speaks with one voice even when several agents do the work. What asks the user or hands them a result is a `Message`, kept and dealt. Everything else an agent says while it works is a `Flash`, seen if the user is there and gone in twenty minutes. Agents are trained to chat, and the flash is where that chat goes without a rule to learn.
costs: the user opens the transcript to see which agent did what, and a flash they did not see is gone.

An agent writes to another agent in a sub-thread or internal message; the user can read it one level down and never gets a `deal.Card` from it.

## bet The thread asks in a message, even when an agent stops
instead of: attention coming from a mixture of mentions from named agents and notices about their state.
serves: Know what wants them, and where they were.
because: the user only needs what asks for them or returns a result, not each agent's progress. A stop or a pending push approval sits right in the thread as another thing to answer, not a separate kind of screen to watch.
costs: a failure can still ask for the user when they have nothing to add.

## bet Flat threads with inline tokens, never projects or channels
status: built code still uses host-side Projects and the separate `Write an idea` (`to:` / `in:` / `on:`) form; migrating to bare threads, inline `:foo` / `:foo <value>` tokens, standing host actors, and auto-registered labels.
decided: 2026-11-28 · delete `Project` as a container/wizard and reject Slack-like channels with shared history; every piece of work starts as a blank `Thread` (flat, or nested via forked tangents), with one inline `:token` syntax for standing actors (`:roboman`), repo/workspace labels (`:rho`), and rare overrides (`:repo …`, `:dir …`, `:model …`, `:host …`)
instead of: repo-locked `Project` containers with a 4-step creation wizard, or Slack-like channels with shared channel history.
serves: Turn ideas into work.
because: a repository is not the natural coordinate system for every piece of work, and channels with shared history turn into a feed to scan; separating *who works* (`:roboman` standing actor/swarm, or a fresh agent when none is mentioned), *where it works / how it is grouped* (`:rho`, `:repo …`, lightweight tags with zero channel history), and *rare runtime overrides* (`:model …`) lets any thought become a thread in one keystroke.
costs: cross-thread defaults live in auto-registered labels and standing actors rather than a single per-project settings record.

## journey An idea becomes a merged result
goal: the user turns an idea into merged work.
starts when: the user has an idea.
steps:
1. the user types on `desk.Desk` (or presses `ctrl-alt-m` from anywhere) with any inline `:tokens` needed, `Start a thread`
2. the user sees the work start, in the thread row's `working` word in `window.Places` (or by filtering `working:`), or by the thread's next message
3. the user opens the thread's ask when it needs something, `deal.Clear what waits`
4. the user answers the ask in its thread, `Answer a message`
5. if the user wants to inspect the full change before merging, they open the diff view (`open diff`) and quote lines into the draft, `agents.Review the change`
6. if merging pushes to `main`/`master` or needs ssh, the user inspects the staged refs/commits/diff right in the thread and approves (`space a`) or denies with words (`space x`), `agents.Approve a push or a fetch`
7. the user opens the result, `deal.Clear what waits`
8. the user looks into the work if they need to see how it was done, `agents.Look into the work`
9. the user reads the result and says done, `deal.Clear what waits`
done when: the work is merged and nothing waits.
day: Turn ideas into work.

## concept Token
status: agreed design not built yet; replaces `Project` addresses and `to:` / `in:` / `on:` headers.
decided: 2026-11-28 · unified inline `:foo` and `:foo <value>` syntax in Insert mode (after whitespace or line start) with live completion and subtle inline highlight; covers standing actors (`:roboman`), repo/workspace labels (`:rho`), plain tags, and value directives (`:repo <url-or-path>`, `:dir <path>`, `:model <runtime>`, `:host <name>`); `:repo <url>` silently auto-registers a short label (`:tide`, or `:org/tide` on collision) with zero wizard/popup
purpose: let the user name who works, which repository or directory to use, lightweight labels, or a non-default model right inside their opening words — with one completion namespace and no separate form.
principle: the user presses `i` on `desk.Desk` and writes `:rho fix the flaky clock test` (starting a fresh agent on `rho` tagged `rho`), or `:roboman :rho investigate the queue drop` (joining the `roboman` swarm on `rho`), or `:repo git@github.com:org/tide.git :model fable-high audit retries` (cloning `tide`, auto-registering `:tide` silently for next time, and starting on `fable-high`).
sees: live completion while typing after whitespace/line-start in Insert mode, and a subtle inline highlight on resolved tokens so the user sees immediately what parsed as a directive vs plain prose (`Note:`, `12:30`, and URLs stay untouched). In Normal mode, `:` still opens `window.Commands`.
does:
- join an actor (`:roboman`, `:ops`, …): attach that standing host actor or agent swarm to the thread; the actor routes/splits work and may apply its own labels (such as `roboman`). Mentioning no actor starts a fresh isolated agent automatically on the default host
- bind a label or repo shorthand (`:rho`, `:tide`, …): attach the label to the thread and, when the label is bound to a host repository or directory, start the work there (cloning a repository into a fresh workset or using the directory). Labels carry **no channel history** — they only bind workspace context, narrow `window.Places` search, and scope `:focus`
- pass a value directive (`:repo <url-or-path>`, `:dir <path>`, `:model <runtime>`, `:host <name>`): consume the next single token as its value. Using `:repo` silently auto-registers a short label from the repository basename (falling back to `:org/name` only on collision) and echoes `started · :tide` so the label is ready in completion on the very next thread
- manage labels (`rename label`, `remove label`, `list labels` in `window.Commands`): tucked-away housekeeping for auto-registered or custom labels
rejected:
- nested colon syntax (`:model:fable-high`) · 2026-11-28 · `:foo <value>` (`:model fable-high`, `:repo git@…`) is easier to type and read
- prompting "Create label `tide`? [y/n]" on `:repo` send · 2026-11-28 · interrupts the one-step start flow; auto-registering silently obeys `The system does what it can`
- Slack-style channels with shared channel history · 2026-11-28 · channels with history turn into a feed to scan; labels stay strictly metadata + optional workspace trigger

## concept Thread
decided: 2026-11-28 · flat top-level threads + nested forked tangents (`↳ N tangents`) with optional tags/actors and no mandatory `Project` container
purpose: keep one piece of work and the whole exchange about it together.
principle: the user starts a thread about a flaky test (`:rho fix the clock test`); the work asks about the test, stages any protected push right in the thread, lets the user inspect its diff (`open diff`), and sends the result in the same thread, whatever agents did the work.
sees: its subject and any attached tags/actors (such as `:rho`, `:roboman`); on a row in `window.Places`, what waits or the latest words when nothing waits (or `<host> away` when its host is away); open, the `Draft` first, any pending push/fetch approval item at the top, then its messages newest first, oldest last.
has:
- many `Message`
- many `Thread`, each a tangent under a message (shown as a quiet `↳ N tangents` line beneath the parent message; `enter` opens them)
- one `Draft` at the top of the thread
does:
- open: one uniform rule everywhere — (1) opening a full-text match from `window.Places` jumps directly to that matched message/line; (2) if a new message, edit, or pending push arrived since the user last viewed the thread, open at the top (`Draft` and newest item); (3) otherwise restore the thread's scroll position and cursor as left
- reply: write in the `Draft` and send with `Message.reply`
- open diff, from `window.Commands` (on any repo-backed thread): open `agents.Change` to read the workset's live diff and quote lines into the `Draft`
- rename thread, from `window.Commands` (inline `:rename New title` or prompt): change the thread's subject
- fork thread, from `window.Commands` on a message: start a tangent `Thread` under that message, quoting it in the new draft
- tag / untag thread, from `window.Commands` (or inline `:tokens` / agent instructions): add or remove lightweight labels on the thread

## concept Message
decided: 2026-11-27 · agent is primary unmarked reading voice in one calm colour whether read or not, user is muted teal with 2px left gutter bar and 10px of air, right-aligned relative ages all quiet, no date separators, no history folding
purpose: carry an idea, an ask, or a result in the thread's voice, keep it as the record of the work, and let the user answer in the thread.
principle: an agent asks a question; the user reads it on top of the thread with earlier words one glance below, and replies without needing to know the writer's name.
sees: the words, with the voice told by colour and gutter alone: the agent's message as the unmarked primary reading voice (one calm `design.reading text`, read or not), the user's own message in muted teal (`design.marked`) with a `2 px` teal left gutter bar; a flush-right relative age on the first line, always `quiet`; a quiet name line above a message from anyone else (or a quiet agent short code when more than one agent has a live message in the same thread); a quiet tag at the end of the text, such as `edited` or `taken back`; what the message quotes. Never inserts date separator lines or collapses older history.
does:
- ping: every agent `Message` pings the user. It is dealt as a `deal.Card` and reads `waits · 2h` on its row. An agent names the kind of everything it sends, `kind="message"` or `kind="flash"`, with no default, so it decides each time whether the user is needed
- look into the work `enter` on an agent message or `Flash` (when cursor is not on a private reply or link): open the `agents.Workset` transcript at the step that produced its current words; a failed stop opens its error
- reply: the user's words go into the thread, and every subscribed actor/agent in it reads them (mid-turn delivery is handled directly by the host harness, so the user does not need a separate cancel step); there is no addressee. With two asks open, the user names the agent in words (or uses an inline `:actor` token to pull a standing actor into an existing thread). If no agent exists yet (a refused start), replying retries using the thread's recorded directives/workspace (falling back to updated label/host settings); if an existing agent is `stopped` or `failed`, replying wakes it in its own workset on its own runtime (preserving any `set agent runtime` choice)
- edit or delete: agents may change or take back their own messages, except that a message the user has answered stays as it was. A real edit reads `edited` and deals the thread again (with a recovered system stop as the one clearing exception, see `agents.a stuck agent asks`)
- reply privately `alt-r`: write a remark directly beneath this message, as in `notes.Reply privately`
rejected:
- a required `## proof` section on every result message · 2026-11-26 · rigid templates fight different kinds of work; let agents back up results flexibly in prose, links, or captures, and use `agents.Change` (`open diff`) or the workset transcript when deeper inspection is needed
- replacing conversational request-response threads with a rigid mutable casefile state machine · 2026-11-26 · back-and-forth brainstorming and follow-ups need real conversation
- kinds of message, `ask`, `result`, and `fyi` · 2026-11-20 · one yes or no, whether the message pings, does the job; a result waits like an ask
- a message that is kept but does not ping · 2026-11-21 · anything worth the record is worth a card, and agents used it to be polite; progress is a `Flash`
- a `## recap` section on a cold message, so it reads without the history · 2026-11-21 · with the newest message on top, the context is one glance below; the recap fold hid whole messages, and agents wrote it badly
- a reply with an addressee: the draft answers one message, says so with an `answers …` line, `r` retargets, and the host delivers the words to that writer only · 2026-11-21 · a chat message has no target; every agent in the thread reads it and the user names one in words when it matters. The deal still settles the card the user came from
- a header with the writer and the time above each message · 2026-11-20 · colour tells the voice, and the top line says how long the thread has waited
- a status line kept apart from the messages, replaced rather than kept · 2026-11-20 · a `Flash`, replaced as the work moves, does the same job without a second surface

Agents can search messages. A message written while the host is away waits on the device and goes out when the host is back. A newly started thread sent offline says `queued` and enters `window.Places` MRU immediately (using its first line until titled) until the host's thread takes its place.

## concept Flash
purpose: let an agent say what it is doing or about to do, for the user who happens to be there, without a card and without a trace.
principle: an agent writes `I plan to drop the cache layer, say no in 20 min` and starts; the user who is in the thread replies `no`, and the agent changes course; the user who is away never sees it, and the thread does not grow.
sees: the words in `design.quiet` under the draft, newest first among the thread's messages, with the age at the end of the first line, `12m`; one open flash per agent, with its earlier fresh flashes folded under it as `+2 earlier`; on the thread's row in `window.Places`, the newest flash's first line as the latest words.
does:
- post, by an agent with `kind="flash"`: show it in the thread for twenty minutes from its first post, longer while it is the thread's newest item from either side; it goes when the clock has run and something newer exists, a message or a flash from anyone; then it is gone from every device, not kept as history
- look into the work `enter` on a flash: open the `agents.Workset` transcript at the step that posted it
- reply: the user's reply is an ordinary `Message` to the thread; it does not keep the flash
- edit: the agent can change its words; the clock runs from the first post
rejected:
- one flash per agent, each replacing the last · 2026-11-21 · they vanish anyway; folding the earlier ones costs nothing and keeps a short plan readable
- an `ephemeral=True` flag with `False` as the default · 2026-11-21 · a default lets the model skip the choice; a required `kind` makes it pick one every time
- an `expires_in` parameter · 2026-11-21 · one clock is easier to trust than a number each agent tunes
- a flash that fades on its clock even while it is the newest thing in the thread · 2026-11-21 · the user who opens the thread an hour later still wants the last state of the work; the next message or flash replaces it
- a soft ask, a flash that pings and takes its card with it when it fades · 2026-11-21 · a card that vanishes breaks `nothing addressed to the user disappears unread`; what needs the user is a `Message`, so the agent names one of two kinds and nothing else
- a kept message that does not ping, for progress · 2026-11-21 · see `Message`

## concept Draft
decided: 2026-11-28 · empty thread draft has zero placeholder text (just the 2px teal left gutter bar and cursor on row 1); new threads are drafted directly on the blank `desk.Desk` canvas with inline `:token` completion (no separate `Idea` screen or `to:`/`in:`/`on:` header)
purpose: keep what the user wrote in a thread (or on the blank `desk.Desk` canvas) and has not sent, so leaving loses nothing.
principle: the user is halfway through an answer (or a new thread on `desk.Desk`) when a card chimes; they deal it and answer it; later, on the same device, they return and the half-written draft is still waiting, even after a restart.
sees: the unsent text in muted teal (`design.marked`), with a `2 px` teal bar in the left gutter, on row 1 at the top of a `Thread` (set apart from the newest message by space alone, no divider line) or directly on the blank `desk.Desk` canvas; when empty in a thread, shows no placeholder word at all — just the teal left gutter bar and cursor on a clean empty line. Inline `:tokens` carry a subtle highlight when recognized. When a thread draft's last line scrolls above the window, the draft becomes a `window.Edge-stuck excerpt` at the top edge, where the whole top zone of the window becomes one borderless canopy in its lifted tone (`design.Draft`); on the phone it is in the half above the keyboard.
does:
- edit `i`, `A`, `o`, or a tap on the draft: write in place; `escape` returns to Normal with the cursor still in the draft
- quote `>` on a selection in one read-only message (or in `agents.Change`): insert those lines with `> ` in front, just above the last draft cursor; resume beneath the quote in Normal (inside the editable `Draft`, `>` stays normal Helix indent)
- send `ctrl-enter`, or `send` on the phone bar while writing: in an open thread, the words become the newest message right below the empty draft with nothing scrolling; on `desk.Desk` (or a blank thread started via `ctrl-alt-m`), starts a new `Thread` as `Start a thread` and clears the draft immediately
- undo `u`, redo `shift-u`: the editor's undo while writing; on the phone, `undo` on the bar
rejected:
- quoting on `enter` when selection width is `> 1` char · 2026-11-26 · Helix motions (`w`, `e`, `b`, `x`, `/`) leave multi-char selections, causing accidental quotes when pressing `enter` on links or messages; `>` is dedicated to quoting on read-only messages
- move the point to the next pinging message after sending · 2026-11-21 · the user stays in the draft; the history moves as needed
- a separator line between the history and the draft · 2026-11-20 · the draft is the next message; a line of chrome on every thread costs more than it says
- a `to:` / `in:` / `on:` header form for starting work · 2026-11-28 · replaced by inline `:foo` / `:foo <value>` tokens directly on the blank `desk.Desk` canvas
- the draft as the last message at the end of the thread, stuck to the bottom while reading · 2026-11-21 · with the newest message first the draft leads, and no open or fill rule is needed

A thread (and the `desk.Desk` new-thread canvas) has one draft on each device, not shared with other devices.

## screen Thread
decided: 2026-11-28 · subject and quiet tags/actors in Thread's top-left title row (no model name, no waits·age; muted / snoozed until … / approve push / working / stopped shown when active), Draft on row 1 with no placeholder, 128ch centered sheet with everything soft-wrapped at 80ch and ages in one column past it, Mono + Balanced, 0px square corners
answers: what is this piece of work waiting for from the user?
shows:
- the `Thread` subject, any quiet tags/actors (`:rho`, `:roboman`), and any active page marker (`muted`, `snoozed until …`, `approve push`, `working`, `stopped by you`, `stuck`) in Thread's top-left title row beside the notch (omitting redundant `waits · age` and model name, which lives in `agents.Transcript`)
- inside a `128 ch` centered sheet (every line, code and tables too, soft-wraps at `80 ch`; the ages stand just past that edge):
- the `Draft` first on row 1 (no `reply` placeholder, `2 px` teal left gutter bar, space alone below), or stuck to the top edge while the user reads history below
- any pending staged push/fetch approval right below the draft (naming repo, refs, force-push dropped count, commits, bounded diff, and `space a approves · space x denies (with draft reply) · space s snoozes`)
- the `Message` newest first, readable and copyable with the same motions and search as the `Draft`, but not editable; agent messages are the unmarked primary voice (one calm colour, read or not), user messages are muted teal with a `2 px` left gutter bar, and relative ages sit flush-right in `quiet`; no date separators and no collapsed history
- the fresh `Flash` of each agent in `design.quiet` with its age flush-right, in its place among the messages, with that agent's earlier fresh flashes folded under it
- a `notes.Private reply` below its message, editable there; tangents appear as a quiet `↳ N tangents` line below their parent message and open as their own `Thread`
states:
- empty: the empty draft, then the opening message; a thread begins with a message
- loading: locally cached messages if available, or `connecting to <host>…` while syncing
- partial: available messages with quiet `<host> away` note in the status line when offline
- error: the thread is unavailable on an unreachable host
- full: follows `Thread.open` — full-text match jumps to that line; unseen message/edit/push opens at top `Draft`; otherwise restores scroll & cursor as left
keys:
- `enter` on a message/item always opens what is under the cursor regardless of Helix selection width: (1) `notes.Private reply` → edit in place; (2) tangent line (`↳ N tangents`) → open tangent; (3) link → `browser.Page.open`; (4) agent `Message` or `Flash` → `Message.look into the work` (workset transcript at that step)
- `>` on a selection in one read-only message: `Draft.quote` (inside `Draft`, `>` stays Helix indent)
- `ctrl-u`, `ctrl-d`: scroll history by half a window without moving the point out of the `Draft`
- `gg`: go to the draft; `G`: go to the oldest message; `i`, `A` or `o`: `Draft.edit`; `ctrl-enter` or `send`: `Draft.send`
- `alt-r` on a message: `notes.Private reply.reply privately`
- `u`, `shift-u` in the draft: undo and redo the last edit
- `space`: silent `window.Screen` prefix (`space f` places, `space u` undo, plus `deal.Verdict` chords when waiting — or `space a` approve / `space x` deny / `space s` → `:snooze thread ` when a push/fetch awaits), anywhere in Normal mode
- `:`: `window.Commands` (including `open diff` on repo-backed threads)
- `space-j` (`ctrl-j`), `tab`, `q`: `window.Screen`
starts: `Answer a message`, `agents.Look into the work`, `agents.Review the change`, `agents.Approve a push or a fetch`, `notes.Reply privately`
captures: [asking message on the laptop](design/laptop-card.png) and [on the phone](design/phone-card.png), [an outcome with its proof](design/laptop-proof.png), [an older thread with history](design/laptop-reading.png), and [the notice in a stuck phone draft](design/phone-reading-notice.png).
rejected:
- the message that pings at the top of the window · 2026-11-21 · open on what has been said since the user's last word, with the next reply ready in either way of opening
- oldest message first, the draft at the end · 2026-11-21 · with newest first and the draft leading, open is always the top and the user's own last message separates new from history
- the agent's transcript beside the message in a second column · 2026-11-20 · one place in focus; the transcript is a flat peer
- the card alone, with the history folded · 2026-11-20 · a short history costs nothing and a long one is one scroll away
- the agent's work folded under each message · 2026-11-20 · the transcript is `agents.Look into the work`
- a split with a thread list at the left · 2026-11-20 · one screen at a time
- marking the message under the point by brightness or a bar · 2026-11-20 · the cursor is enough

The point never rests on the blank line between two messages. A reply from a waiting thread (without a pending push/fetch) settles the visible waiting card whether the thread was opened by the deal or by name, and stays on the thread; the next card waits for `deal.Card.deal`.

## flow Answer a message
status: the user's Slack is in the browser, and the bot is not built.
runs: 100 a day
day: Keep the work moving.
when: an agent thread or the Slack bot has something that asks for the user, or the user opens a result
want: read what arrived and answer in place if needed
so that: the person or agent that needs the user's words gets them, and the message no longer waits
steps:
1. the user opens the thread by the deal or by name and reads what arrived since their last message
   - when there is nothing to say, the user gives `deal.Verdict.done` and stops here
2. the user writes and edits an answer in the `Draft` at the top of the thread
3. the user sends the answer with `Message.reply`; the reply settles the waiting card on this device even while it waits to be delivered. If the host refuses it, the card returns and the unsent answer stays in the thread, marked `not sent`; `write reply again` in `window.Commands` takes it back into the empty `Draft` to change and send again.
system: opens at top `Draft` when new messages arrived (else restores view), with messages below newest first. The user's last message separates what arrived since they wrote from earlier history. The thread keeps the draft if the user leaves. The reply is delivered by the host harness to every subscribed actor/agent in the thread, waking any stopped/failed agent in its own workset & runtime (or retrying a refused start using the thread's recorded workspace & runtime).
user: the words of the answer, or done when there are none.
cases: a question answered after a day away, with earlier words one glance below; a result with proof; a colleague reached through `slack.Bot`; replying after returning via `window.Places`; replying while the host is away (queued offline).

## flow Start a thread
status: built code currently uses a separate `Write an idea` screen with `to:` / `in:` / `on:` headers and `Project` setup; migrating to blank-canvas `desk.Desk` + inline `:foo` / `:foo <value>` tokens.
decided: 2026-11-28 · start work by typing directly on `desk.Desk` (`i`) or jumping to the blank draft via `ctrl-alt-m` from any screen; inline `:foo` / `:foo <value>` tokens handle actors, repo/workspace labels, and rare overrides; no `Project` wizard and no `Idea` form
runs: 12 a day
day: Turn ideas into work.
when: the user has an idea or task, on `desk.Desk` or anywhere else in the window
want: turn it into running work in one step without picking modes or filling out a form
so that: work starts immediately and the user can carry on where they were
steps:
1. on `desk.Desk`, the user presses `i` (or `ctrl-alt-m` / `new thread` from any other screen to jump to the blank draft, reopening any unsent new-thread draft on this device)
2. the user writes in the usual Helix modes, optionally using inline `Token` directives with live completion:
   - `:roboman` (or another standing actor) to join that swarm/actor to the thread; omitting any actor starts a fresh isolated agent automatically
   - `:rho` (or another known label/repo shorthand) to bind that workspace and tag the thread
   - `:repo <url-or-path>`, `:dir <path>`, `:model <runtime>`, `:host <name>` when pointing at a new repo, directory, non-default model, or non-default host
3. the user sends with `ctrl-enter` or `send`; the thread queues durably, enters `window.Places` MRU immediately (using its first line until titled), the draft clears immediately so another thread can be written even while offline, the echo names the started thread (and any newly auto-registered label such as `started · :tide`), and `desk.Desk` returns to calm ambient stillness (or the previous screen returns when started from elsewhere via `ctrl-alt-m`). If a queued offline start is later refused by the host before a host thread exists, it surfaces locally as a `not sent` card with `write thread again` in `window.Commands`
system: parses inline `:foo` and `:foo <value>` tokens, routes to mentioned standing actors or starts a fresh isolated agent on the default host (in a clone of the named repo, the named directory, or a scratch workspace), silently auto-registers a short label when `:repo` is given, and preserves any explicit tokens for retry if the start fails.
user: the words and, when non-default, any inline `:token` directives.
cases: a plain thread with no tokens (fresh general agent); `:rho` tagging + cloning `rho`; `:roboman :rho` joining a standing swarm; `:repo git@github.com:org/tide.git` auto-registering `:tide` silently; `:model fable-high` overriding runtime inline; an unsent new-thread draft preserved on `desk.Desk` after jumping away with `space-j` or restarting; writing a second thread while the first is still queued offline; a refused unknown runtime keeping the draft intact.
rejected:
- a separate `Write an idea` screen with `to:`, `in:`, and `on:` header lines · 2026-11-28 · typing directly on the blank `desk.Desk` canvas with inline `:tokens` eliminates the separate screen and form
- a 4-step `Start a project` wizard asking place, mode (`one`/`per idea`), and runtime · 2026-11-28 · replaced by standing actors (`:roboman`), auto-registered repo labels (`:rho`), and inline `:model` overrides

## rule the last message stays live
when an agent has already sent a `Message` since the user's last message in the `Thread`, then rho refuses another from that agent, tells them which message to edit, and points them at `kind="flash"` for anything that does not need the user. Other agents can each send one message. A `Flash` never occupies the live message slot. A message that was deleted or taken back no longer occupies that place. A real edit reads `edited`. An unchanged ask does not deal again after `done`; changed words do (with a recovered system stop as the one clearing exception). A reply is read by the thread's agents and settles the thread through the latest message visible when the user replied, freezing the messages already answered. A different question forks under the message that raised it. An agent-to-agent message stays in the thread without lighting the deal.
why: a thread of ten messages that each supersede the last is a feed; each agent has one place for current words, and the flash is where the rest goes.
rejected:
- a second send merges in place · 2026-11-21 · the agent has to choose to edit its existing message; another send must not silently change it
- a cold message carries a `## recap`, and rho reminds the agent when it does not · 2026-11-21 · see `Message`

## rule new messages land below the draft
when new `Message` arrives, the user's own or an agent's, or the newest message is edited, then it takes its place right below the `Draft` and the point does not move. With the window at the top, the user sees it at once and older messages move down. With the draft stuck to the top edge, the view stays where it is, and for an agent message the stuck draft shows a quiet `↑ new` at its right end until the user scrolls back to the top. Until then the user has not seen it: a reply or `deal.Verdict.done` leaves it waiting.
rejected:
- history pushes up to keep the draft in place at the tail, `↓ new` for an agent message when stuck at the bottom · 2026-11-21 · the mirror of this rule for the oldest-first order
