# Desk, Rooms, and the Dealer

A design for how rho manages attention: switching between things, remembering
things, and deciding what to work on next. This covers the desk, browser
pages, agents, chat, and the window manager direction.

This is a design, not a spec. It records the *why* behind each decision.
Exact behavior should be decided later, guided by these whys. Rho has exactly
one user, so everything here is tuned to that one person and can be changed
the moment it stops fitting.

## The problem

The desk works well as a todo list: it is the human's context window paged
out to a document. But around it there are frictions:

- Switching between 2-3 active things means finding the right item in the
  desk every time. Too slow for something done dozens of times per hour.
- Creating a browser tab requires picking a desk heading first. You must
  classify a page before you have even looked at it.
- Agents that owe a reply get forgotten, ideas from twitter / chat / walks
  get lost. There is no cheap way to write things down and trust they come
  back.
- Browser tabs pile up because closing one feels like losing it. There is
  no "done" or "later" for a tab.

The design goal: one system where everything pending lives in one place,
things come back on their own, and switching is a reflex instead of a search.

## Core decisions and why

### The desk is 100% user-written

Every line of the desk is there because the user put it there.
The system never writes into the desk, never reorders it, never rewrites it.

**Why:** the desk is a paged-out memory. A memory is only useful if it is
stable — if the system rewrites the document, reading it no longer tells you
what you decided, it tells you what some process last did. A desk the system
co-writes becomes a feed you read instead of a memory you own. This rule is
structural: it is not "the system is careful", it is "the system cannot".

### Rooms: top-level headings are categories

Top-level headings are rooms: "rho", "work", "discord", "social". A handful
of them, alive for months. Everything lives inside some room — agents, pages,
files, chat conversations. Actual tasks and projects are sub-headings inside
a room. Rooms can end: archiving a top-level heading is how a room dies
(this already exists).

**Why:** switching happens between a small number of stable contexts, not
between dozens of tasks. Because rooms are few and long-lived, recency-based
switching stays calm and spatial memory inside a room has time to form.
"Everything lives in a room" keeps the model to one noun: there are rooms,
and there are things inside rooms. Nothing is homeless — social chat just
lives in the "social" room.

### The inbox is outside the desk

New things land in an inbox that is *not* part of the org document: captured
thoughts, new browser tabs, coworker pings, ideas. Inbox items are structured
(text + source reference + captured context + timestamp), owned by the
machine. A new browser tab is not a special kind of item: it is an ordinary
capture whose source references the page. The user triages inbox items into
the desk by hand. It can be *rendered* to look like part of the desk, but
underneath it is a different store.

There is no time-based expiry anywhere. Nothing is machine-deleted. Items
leave the inbox only through a user verdict (file or discard), or when the
external thing they reference resolves itself (a chat thread answered on the
far side retires its own item). An inbox item is *waiting to be triaged*, so
its card rises over time until the user confronts it: the inbox drains to
zero through verdicts, never through fading or deletion. Fading curves
belong to filed content (a `:reminder:` on the desk), not to triage debt.

**Why:** two territories with a membrane between them keeps both honest.
The machine can freely create, update, and retire things in its own
territory. The desk stays fully deliberate because the only path in is
through the user's hands. Arrival is not commitment: a ping from a coworker
is a question ("is this yours?") until the user files it.

**Why no expiry:** deletion is a verdict the machine has no right to make.
Every capture gets confronted eventually (its card rises until then), and
the user's file-or-discard is what empties the inbox. Machine deletion buys
nothing (storage is free) and breaks the trust that nothing written down is
ever lost.

### Capture costs nothing and decides nothing

One gesture, write the thought, back to what you were doing. No picking a
heading, no category, no naming. The system silently attaches context: what
was on screen, the current room, the source (page URL, transcript position,
chat message), timestamp. Sources: reading pages, talking to agents ("note
this down"), coworker pings (captured automatically), and voice via Iris
when away from the keyboard.

**Why:** every decision added at capture time is paid on every thought, and
eventually you stop capturing. The brain lets go of a thought only when it
trusts the system to bring it back (GTD). Filing happens later during
dealing, where the attached context makes the right room obvious. The
attached context also fixes the "what did I mean by this?" problem: a lazy
note is fine at write time because the system replays the scene at read
time.

### One lifecycle for everything

Everything — a tab, an agent, a task, a whole room — is pending, active,
deferred, or done. Verdicts differ per kind: done on a tab is a dismissal
and must cost nothing; done on an agent is accepting reviewed work and
deserves friction; deferring a room ("rho sleeps until tonight") mutes
everything inside it.

**Why:** one lifecycle means one mental model and one dealer for everything.
Tab hoarding is what humans do when tabs lack done/defer semantics — the tab
stays open because closing it loses the commitment. Giving pages the same
lifecycle as agents fixes that: done archives (recoverable), defer resurfaces
later. But the *word* is shared, not the meaning, so the gestures and the
amount of friction differ per kind on purpose.

### The dealer: a cooperative scheduler for attention

The dealer hands you the next card: an agent that finished, a ping, a
deferred task that woke up, an inbox item to file. Skip is always free —
one gesture, no guilt, no explanation. The dealer never interrupts: urgency
shows as signals (lamp, chime), never as the screen changing on its own.

Ranking is by curves: every item has an urgency curve over time, and the
kind sets the shape. Pings rise (someone is waiting, the cost of silence
grows). Unfiled captures rise slowly (filing is a small debt). Finished
agent results and reminders fade (pure information: ignored for a few days
means implicitly accepted — and never deleted).
Deadlines spike near the date. Deferred items are zero until wake time.
Cards in the current room get a small bonus — enough to prefer staying
put, never enough to bury something urgent in another room. Context
switches cost, so the dealer leans against them, but only leans.
Dealer policy is written as properties on desk headings (`:defer:`, time
windows), so the dealer can always answer "why this card?" by quoting the
desk back.

Below a cutoff, cards are simply not dealt. The empty state is a success:
"nothing needs you" is a valid and good answer.

**Why cooperative:** interruption research is brutal about the cost of
being context-switched by a machine. Pressure yes, preemption never.

**Why curves:** a total order is needed for dealing, and curves are simple
enough for the one user to understand and tune directly. The score is
internal — the explanation shown is the inputs ("this ping is two days
old"), not the number. Tiers between kinds are just a curve shape (a step),
so no separate mechanism is needed.

**Why the cutoff and the proud empty state:** "next" is a variable-reward
pull; a dealer that always has a card trains compulsive dealing — the same
check-loop that badges and feeds use, rebuilt inside the tool meant to kill
it. The deal's real asset is signal-to-noise: every junk card devalues all
future cards. The dealer can afford to say "nothing" because the human
returns on their own anyway — it is not responsible for keeping you engaged.

### Navigation: a timeline gesture, a strip, and an overview key

Between rooms, one vertical timeline: up steps backward through
recently-used rooms (the stack visible while stepping), down opens the
dealer. Dealing is not room switching: taking a card in the current room
just loads it into the strip, and only a distant card moves you. Skipping
is one key and costs nothing but the honest record of a skip.
The desk itself is a separate overview key — like the windows key — that
zooms out to the map, landing on the current room's heading.

**Why:** most switching is going back to something recent, and that must be
a reflex, not a search — the original friction was exactly "finding the item
in the desk". The vertical axis is time, not space: past above, future
below. Blind MRU cycling (alt-tab confusion) is avoided because the scrub
shows the stack. The desk stays the map: it is consulted deliberately, its
order is never auto-changed (auto-reordering destroys spatial memory — the
old adaptive-menus mistake), and landing on the current heading quietly
teaches where you are on the map.

### Inside a room: the strip is a dynamic working set

A room shows a horizontal strip of surfaces (niri-style): agents, pages,
terminals, chat. The strip is *not* a static layout and *not* derived from
the org tree. It is the live working set: taking a card from the dealer
loads its surface into the strip, touching something loads it. Removal is
manual — one cheap gesture. Nothing is lost on removal: the desk retains
everything; the strip only shows what is in hand.

**Why:** loading must be automatic — the brain does not file the three
things it is juggling, they are just "in hand", so the dealer and plain
touching fill the strip with zero ceremony. Removal is deliberate because
dropping something from the working set is a small verdict, and a timer
makes that verdict wrongly in both directions: it drains the thing you
were about to come back to and keeps the thing you silently abandoned.

### Log everything, adapt nothing silently

The journal is the whole-GUI interaction record, not a dealer log. Every
user interaction goes through it — nothing bypasses the journal: deals
(card, kind, curve score, room, time of day, verdict, time-to-verdict, and
the cards considered but not dealt), inbox verdicts wherever they happen,
captures, manual desk lookups ("find events"), opening and viewing agents,
surface and pane switches, scrolling. Storage is free. The point is wider
than dealer tuning: any future rho-gui UX decision should be able to lean
on data about how the GUI is actually used.

The logs are for measurement and proposal, never silent adaptation. Skip
rate per kind shows which curves are wrong. Find events are the dealer's
misses (a card it should have dealt). Slow skips mean the dealer was close;
fast skips mean it was obviously wrong. Changes to curves happen as
proposals the user approves, informed by the data.

**Why:** single-user means every log entry is a perfectly labeled example —
the thing products can never have. Logs make curve tuning empirical instead
of vibes, and enable *replay*: a candidate v2 dealer can be evaluated
offline against recorded history ("would it have dealt what I engaged and
buried what I skipped?") before anyone lives under it. Silent adaptation is
banned because it destroys the "why this card?" answer — a dealer that
quietly reweights itself is the feed again. In v2, logs may feed the dealer
as *fitted parameters inside the legible curve structure* ("you never touch
rho before noon" becomes a visible window rule), never as an opaque model.
One recorded danger for v2: dealer-deals-what-you-engage is a feedback loop
(a filter bubble of one); the standard cure is a small, clearly-labeled
exploration budget.

## The GUI shape (v1)

The dealer world inverts the GUI. Today the desk buffer is home: you live in
the document and open things from it. In the new shape you live in a room and
things arrive. Three layers:

**Ground: the room strip.** The screen is the current room — a horizontal
strip of surfaces (agents, pages, shell, zulip), one surface full-screen at a
time. The strip slides with a normal horizontal scroll — the focused surface
gets the gesture first, and at its edge it spills into the strip — with
keyboard keys as fallback. It never wraps, and each room remembers which
surface you were on. A small room name in a corner is the only chrome. No splits: one surface per strip cell. Splits are complexity
paid on every layout decision, and sliding between full-screen cells is cheap
enough to cover the side-by-side cases for now. They can come back later if
their absence is actually felt.

**The timeline.** One axis, two directions, keyboard-driven for now: one
key steps back through recently-used rooms (the stack visible while
stepping — stop tapping and you are simply there), the other opens the
dealer. Dealing is global and item-level: one queue across all rooms, with
the small current-room bonus from the dealer section. A card in the current
room just loads into the strip; a distant card switches rooms. Room-level
verdicts (defer the whole room) are given from an item card. Inbox captures
are ordinary cards in the same queue.

**The overview.** The desk map is the current rail view kept as it is —
the desk buffer with live agent rows, attaching, editing, filing all work
the same. What changes is its role, not its looks: it is a place you visit
deliberately (the overview key), not the place you live. The rail's
aggregation of everything exists only here; there is no persistent
all-agents list anywhere else on screen.

**The deal is full-screen.** A deal is one decision. The card takes the
whole screen — content preview, the "why" in plain inputs, verdict keys —
so there is no half-attention split between the card and the room behind
it. A freshly dealt surface is in a vim mode of its own, DEAL, which is
normal mode plus a few verdict letters — motions, search, scrolling all
work, and there is nothing to switch in or out of. Accept is the default:
staying on the card, typing into it, or moving on to the next deal all take
it. Skip is its own explicit key. A shown card is provisional until you
leave it: skipping leaves no trace but the skip itself, no strip entry, no
room switch. The dashboard as a destination dies: the deal is a moment, not
a place.

**Signals push dirtiness, never changes.** No event gets its own signal:
things only mark the dealer's hand dirty, and the signals are derived from
the hand's current state. Two thresholds: a quiet visual cue (a translucent
shade in a corner) at a lower bar, a chime at a higher one. The chime is
edge-triggered — it rings once when the hand crosses the line, and again
only after the hand has dropped back below. Signals carry no content: no
counts, no names. A count is a badge, and a badge is the checking loop.
Pulling (opening the hand) is the only way to see what or why. Agents you
recently interacted with get a fading bonus on their cards, so a completion
from an agent you are actively driving reaches you immediately while a
background agent's completion waits quietly — as a curve bonus, not a
signal special-case, so dealing order agrees with the signals.

**Cold start boots into the desk.** Sitting down starts at the map: orient
first, then dive into a room or ask for a deal. This also keeps the rule
that the dealer never moves you anywhere on its own — including at startup.

## Deliberately deferred

These were discussed and parked on purpose. None of them block the core;
all are additive later:

- **Briefings** — an LLM summary at wake / on returning to a cold room
  ("while you slept: two agents finished, three pings arrived").
  Generated from the logs, so it can be added any time.
- **Agent help with filing** — an agent suggesting the room for an inbox
  item, or gardening the desk on request. Manual filing first.
- **Rituals / sweeps** — a cross-room "drain everything" mode, morning or
  evening shapes. Cannot be designed before living with the dealer, because
  current habits are shaped by the dealer not existing.
- **v2 learned dealing** — see logging section for the rules it must obey.

## Symptoms to watch for

- **Curve whack-a-mole:** every tuning fix breaking another comparison,
  week after week. The remedy: flatten cross-kind comparison into coarse
  steps, keep curves within a kind.
- **Slot-machine drift:** reaching for "next" compulsively, dealer rarely
  empty. The remedy: raise the cutoff.
- **Desk turning into a feed:** if any future feature wants the system to
  write into the desk, this document's first decision is the answer: no.
  Machine text belongs in the inbox.

## What "v1 done" means

The user daily-drives the dealer. The stated bar: if the design above
exists, daily driving happens. So v1 is exactly this design, and any cut
that breaks daily-drive is a cut too far.
