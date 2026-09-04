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

Decided with the user on 4 Sep. Deal mode goes away entirely: no `VimDeal`
context, no `DEAL` status word, no single-letter verdict keys on a surface,
and `escape` means nothing to the dealer. Vim is vim on every surface, so
a card can be read, searched and yanked like any buffer.

The verdicts live in one transient. A single tap of `shift` opens it on
any surface that is a card (an agent, a Slack conversation or thread, a
note, a page): `d` done, `x` discard, `s` snooze then a count and a unit
(`s 7 d` is seven days, `s 45 m`, `s 3 h`, `s w`, `s s` a day), `t` todo,
`f` file…, `u` undo the last verdict, `j` open the top card (what `ctrl-j`
does), and `shift` again for Home, so the old double-shift still lands on
Home. `escape` closes the transient and nothing else. The writes, undo,
journal and the status-line label (`needs reply · 2h`) are exactly what
deal mode did; only the keys moved. `ctrl-j` keeps opening the top card
as an ordinary surface. The phone keeps its buttons and sheet.

**Why:** the user's words: deal mode stole letters from reading, made
`escape` a verdict, and hid what the keys were. One transient shows the
verdicts, reads as a menu, and leaves vim alone. `shift` is the one key
that is free on every surface, in every mode, and already means "rho,
not the editor" through the Home double-tap.

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

## Deliberately deferred

- Editing anything from Home.
- Per-section keys beyond open and deal.
- GitHub rows (arrive with the GitHub integration as ordinary cards).

## What done means

Sitting down shows Next, Running, Later in one glance without dealing;
opening a row is a deal in every respect; an agent's last line updates
live; the periphery scrolls in only when asked; a screenshot per state
(busy, empty, phone) from fakes and the dealer's test fixtures.
