# Home

Status: decided with the user on 2026-09-03, built the same day
(`crates/rho-gui/src/home.rs`). Changes the cold-start and overview rules
of `DESK-DESIGN.md`; the dealer itself is untouched. As built: the map
keeps a root-menu key of its own (`o map`); the overview key pressed on
Home returns to the surface the reader came from.

## The problem

The desk was home: cold start lands on the map, "nothing to deal" lands on
the map, the overview is the map. But the desk is a notes store, and most
of the day is not notes. What is missing is a glance: what is running, what
is coming up next, and what sits just under the line. Today the only way to
learn any of that is to deal, one card at a time.

## Decisions

### Home is a window onto the dealer's own ranking

One buffer, one surface, called Home. It shows the dealer's list with the
cutoff drawn as a line. Nothing is scored twice: the rows are the same
cards, in the same order, with the same words the deal bar uses (`needs
reply · 1.9h`, `finished · 2.0d`). Slack threads, agents, pings, and
captures are all just cards, so channels need no section of their own: a
thread waiting on a reply is above the line, a channel with mere chatter is
below it.

An agent created by an agent belongs to its creator. It is not dealt, not
on Home, not in the running list, and not in Find; its waiting reaches the
reader only through its creator's card. Only agents the reader created
directly are theirs to deal with.

### Order: Next, Running, Later

```
next
  #design › release date      needs reply · 1.9h
  eng-b8os                    finished · 40m
  capture: try the new flake  unfiled · 3.0d

running
  eng-5pha   phone feed        12m   "wiring the flick recogniser"
  eng-b8os   slack polish       3m   "unfurl box: background tint"

later
  #random                     quiet · 5.4d
  eng-qeo0                    finished · 6.0d
```

- **Next** is the top of the queue above the cutoff, capped at a handful of
  rows (5). A preview of what is coming, not the queue.
- **Running** is every live agent: name, what it is on, elapsed, its last
  output line, updated live from the transcript subscriptions.
- **Later** is the rows just under the cutoff, muted, capped the same way.
  Peripheral vision: enough to know what is around, not enough to groom.

Later comes last on purpose: when Next and Running are tall it falls off
the bottom of the screen and is reached by scrolling, so the periphery
costs nothing when the foreground is busy.

### Dealing stays the act

Any row opens as a deal through the dealer: the surface, verdict keys,
undo, and the timeline behave exactly as if the card had been dealt. `ctrl-j`
from Home deals the top card as anywhere else. Home never closes anything
itself; it has no verdict keys of its own.

### Home is where empty lands

Cold start lands on Home. "Nothing needs attention" lands on Home. The
overview key opens Home. On the phone, Home is the card after the last
deal: flick past the queue and it is what you see.

### What Home does not do

- No counts leak out of it: the lamp and chime stay contentless.
- No scrolling into the whole queue: the caps are hard, so Home cannot
  become an inbox to tidy.
- No pronoun: the word "you" does not appear; sections are `next`,
  `running`, `later`.

### Built on the transcript primitive

Home is one keyed incremental transcript (`crates/rho-transcript`): each
row is an item keyed by card identity or agent id, so a score change,
an agent's new output line, or a card crossing the cutoff edits only its
row. The dealer's invalidation is the trigger; nothing polls.

## The desk after Home

The desk loses cold start, the empty landing, and the overview job. What
remains is notes and filing: a note attached to a room, an agent, a
channel, or a repository, opened from that thing with one key, and the tree
as the store that agents file into. That is a smaller design and gets its
own pass once Home exists; until then the map stays reachable from Home.

## No deal mode: one key opens the verdict transient

Built 4 Sep (b8os): one key opens the verdict menu over any card
surface, the same key again is Home, snooze takes its count and unit
inside the transient with the pending count drawn beside the title, and
`context_area` now delegates to `surface_node` so a Slack channel or
conversation surface answers the key (it had only answered on threads).
Label is not in the transient; filing by label is `f`.

The key is `tab` (8 Sep). It was a tap of `shift` until the user found
the modifier distracting; a tap also needed the platform's modifier
events and a hold timer to tell it from a chord, and `tab` needs neither.
On the draft `tab` still walks the fields. Over a surface that is no card
it goes Home in one press, and from Home it is the way back.


Decided with the user on 4 Sep. Deal mode goes away entirely: no `VimDeal`
context, no `DEAL` status word, no single-letter verdict keys on a surface,
and `escape` means nothing to the dealer. Vim is vim on every surface, so
a card can be read, searched and yanked like any buffer.

The verdicts live in one transient. `tab` opens it on any surface that is
a card (an agent, a Slack conversation or thread, a note, a page): `d`
done, `x` mute, `s` snooze then a count and a unit (`s 7 d` is seven
days, `s 45 m`, `s 3 h`, `s w`, `s s` a day), `t` todo, `f` file…, `u`
undo the last verdict, `j` open the top card (what `ctrl-j` does), and
`tab` again for Home. `escape` closes the transient and nothing else. The writes, undo,
journal and the status-line label (`needs reply · 2h`) are exactly what
deal mode did; only the keys moved. `ctrl-j` keeps opening the top card
as an ordinary surface. The phone keeps its buttons and sheet.

**Why:** the user's words: deal mode stole letters from reading, made
`escape` a verdict, and hid what the keys were. One transient shows the
verdicts, reads as a menu, and leaves vim alone. `shift` is the one key
that is free on every surface, in every mode, and already means "rho,
not the editor" through the Home double-tap.

## No hand: every pull ranks fresh, and skip is a cursor in memory

Decided with the user on 4 Sep, after the store landed and the GUI
"went down a ton": there is no deal session, no deal queue, and no
hand object anything reads from. Each `space-j` (and `ctrl-j`) is one
pull: rank everything open now by the rules and open the single most
important card as an ordinary surface. A pull taken while a card
surface is in view skips that card first, which is what keeps a pull
from returning the same card. A verdict acts on the card of the surface
in view, or the map cursor row when the map is open, and is a fact write
plus the journal; every view re-derives from the facts after it. The
status label derives from the card in view. The phone sheet derives the
same way.

Skip is modelled like the done cursor, in memory: `(id, cursor, at)`,
the cursor being the source's own position, the same one `d` writes (the
Slack unit's newest ts, the agent's event position, a mark's time). A
card is skipped while nothing on it is past that cursor and the cooldown
(15 minutes, one named constant) has not run out; the "past the cursor"
test is the one the Slack card already uses. Not synced, not stored;
there is no fingerprint. Home renders the same fresh ranking with
skipped rows shown and marked, never hidden, so Home is the truth and
never reads the dealer's filtered output.
Built 4 Sep (367ce0fb, b8os): `DealSession`, the hand, `deal_mode`,
`skip_and_end_deal`, the considered-not-dealt bookkeeping and the
fingerprint string are gone; skip is `(id, CardCursor, at)` with a typed
cursor per source; status label, desktop frame, phone sheet, feed and
flick all read the card in view. The journal's `DealMode` variants stay
so old journals still decode. Landed after it (0707dff5): space-j never
steps forward through history (the forward step and its two tests are
retired; reopening is the buffer picker or the list), and a Home row
under the cursor is the card in view for every verdict, with Home
refreshed in place rather than closed, while a pull from Home opens the
top card without passing over the row.

Mark-read-before an age (`space shift-s m`) also writes
`SlackHandledThrough` at the cutoff for every unit it covers, one
verdict each, undone as one (the user: no Slack read cursor in dealing,
"handle mark read on our side"). Landed 4 Sep (d84ab832).

**Why:** the user's words: "each pull with space-j should give the most
important thing based on rules", "no extra in-deal-mode state, no deal
queue, everything is fresh", and "skip semantics were pretty useful".
What was found on the way (b8os, 4 Sep): Home did redraw on every
change; what made it read as empty was that it rendered the hand with
skipped cards filtered out for the cooldown, so a few pulls emptied Home
while the dealer kept handing the same backlog around.

## Snooze takes a unit

Decided with the user on 3 Sep (first recorded in DESK-DESIGN, which is
retired; restated here because the dealer lives here now). The snooze key
is an operator: `s` followed by a unit, with an optional count in front,
vim style. `45sm` is 45 minutes, `3sh` three hours, `2sd` two days, `sw`
one week, `ss` the default of one day. No prompt, no minibuffer; the deal
bar echoes the resulting time. On the phone the sheet offers chips:
`30m · 2h · tonight (18:00) · tomorrow (09:00) · 3d · next week`. The
agent snooze prompt goes away; agents take the same operator. Snoozing
writes `defer_until` with pace 0 (STORE-DESIGN), so the card comes back
exactly then and rises from zero.

**Why:** snoozing is done dozens of times a day and most of them are
"not now, an hour", which a prompt makes slow and a day-granular key makes
wrong. Count-plus-unit is already in the user's fingers from vim.

Landed 3 Sep. `s` is an operator: `sm`, `sh`, `sd`, `sw` and `ss` (a day),
with vim's count in front, and `s` on its own waits for the unit rather
than snoozing. Minutes and hours land on the clock (millisecond precision,
so a card can come back this afternoon); days and weeks land on a date, as
a defer always has. The bar says the time it comes back on (`snooze until
22:54`, `snooze until Sat 5 Sep`), and the map's mark hint carries the
clock time with it. The phone's defer button opens the chips instead of
taking a day; the agent snooze prompt is gone. The pace follows in the
next change: `Verdict::Defer` writes `pace_days` 0 beside the wake time, so
a snoozed todo comes back from zero rather than halfway up its old curve.
The pair is one shape in `rho-desk`, which the writer builds and the daemon
checks, so an entry naming only the wake time is refused.

## Done is a cursor; mute and snooze are not

Decided with the user on 8 Sep. The three verdicts differ in what a later
message may do to them:

- **Done** says "up to here". It is a cursor, so anything past it is news
  and brings the card back. That is what makes `d` safe to press on a
  thread that is still alive.
- **Mute** says "not this thing". Nothing arriving takes it back: not a
  Slack reply, not an agent starting a turn, not an agent's turn ending
  with a question. Opening the thing is what clears a mute, because that
  is the user looking at it again.
- **Snooze** says "not until then". Nothing arriving shortens it either;
  the wake time is the only thing that ends it. The cursor is left alone,
  so the messages the user never handled are still theirs when it wakes.

The rule is applied per source and in one place per surface. A Slack unit's
card reads its state before anything else; an agent's attention answers
Quiet for a muted agent whatever its turn is doing; and Home's running list
asks the same question the dealer asks per card — has the user put this
agent away, by a mute or by a snooze still ahead — instead of listing every
agent with a turn in flight.

**Why:** the user's words. A snooze that the next reply voids means "not
until Monday" reads as "until somebody writes", and a mute that a running
turn overrules means the thing you put away comes back by moving. Only
`done` is about a position in a stream; the other two are about the thing.

## There is one mute, and `hide` is gone

Decided with the user on 8 Sep. `shift-d` "hide" wrote the same
`DeskVerdict::Mute` as `x`, so rho had two names, two keys and two words in
the log for one verdict — and the second name spread: an agent was "hidden"
in `rho-agents` and muted everywhere else, which is how lists came to filter
one and not the other. The hide entry, `Command::AgentHide` and the hide
half of the agent-done key are gone; `x` on the card is the mute, and the
filing rho reads is `agent_muted`. A muted agent is left out of every list
that draws agents — Home's running list, the finder, and the draft's start
field — while its handle still resolves and its row is still on the map,
which is where the mute is taken back.

## Deliberately deferred

- Editing anything from Home.
- Per-section keys beyond open and deal.
- GitHub rows (arrive with the GitHub integration as ordinary cards).

## What done means

Sitting down shows Next, Running, Later in one glance without dealing;
opening a row is a deal in every respect; an agent's last line updates
live; the periphery scrolls in only when asked; a screenshot per state
(busy, empty, phone) from fakes and the dealer's test fixtures.
