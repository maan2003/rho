# Notes and filing

Status: retired to its notes-and-filing half on 2026-09-03 (tree slice 4).
What used to live here has moved: the store and the shape of a node are
`TREE-DESIGN.md`, the dealer and what it ranks are `HOME-DESIGN.md`, the
surface timeline and the deal are `HOME-DESIGN.md` too. The desk, the
inbox, and rooms are gone as concepts; this is what survives them, which is
how notes are written and how things get filed.

This is a design, not a spec. It records the *why* behind each decision.
Rho has exactly one user, so everything here is tuned to that one person and
can be changed the moment it stops fitting.

## Note text is 100% user-written

Every word of a note body is there because the user put it there. The
machine creates reference nodes, moves them when a verdict files them, and
sets state; it never writes a `body`.

**Why:** a note is a paged-out memory. A memory is only useful if it is
stable — if the system rewrites it, reading it no longer tells you what you
decided, it tells you what some process last did. Notes the system
co-writes become a feed you read instead of a memory you own. This rule is
structural: it is not "the system is careful", it is "the system cannot".

## A note is one text; its first line is its title

The body is multi-line and as long as the thought needs (the store caps it
at 4 MiB, which no thought reaches). The first line is what every other
view calls it: a path segment in find, a card's label, a row on the map. No
title is stored anywhere, so a note renamed by editing its first line is
renamed everywhere at once.

**Why:** a stored title is a second copy of the same fact, and two copies
drift. Deriving it also means writing a note costs one gesture: type, and
the first line has already named it.

## Capture costs nothing and decides nothing

One gesture, write the thought, back to what you were doing. No picking a
parent, no category, no naming: a capture is a note at the root. Filing
happens later, when the context makes the right parent obvious.

**Why:** every decision added at capture time is paid on every thought, and
eventually you stop capturing. The brain lets go of a thought only when it
trusts the system to bring it back (GTD).

## Filing is the parent, and "notes for this" is one key

A node's parent is its context, and that is the whole filing system. A note
under a Slack thread is notes for that thread; a note under an agent is
notes on that agent's work. One key on any surface opens the note filed
under whatever is on screen, creating it the first time.

**Why:** a note about a thing wants to be found from the thing, not from a
folder that happens to be named after it. Since the parent is the only
place filing lives, "notes for this" is a lookup rather than a feature: the
child note of the node the surface is about.

## One lifecycle for everything

Everything — a page, an agent, a thread, a note — is open, deferred, done,
or dismissed. Verdicts differ per kind: done on a page is a dismissal and
must cost nothing; done on an agent is accepting reviewed work and deserves
friction; deferring a parent mutes everything inside it.

**Why:** one lifecycle means one mental model and one dealer for
everything. Tab hoarding is what humans do when tabs lack done/defer
semantics — the tab stays open because closing it loses the commitment.
Giving pages the same lifecycle as agents fixes that. But the *word* is
shared, not the meaning, so the gestures and the amount of friction differ
per kind on purpose.

## The map and the note surface

The map is the tree by kind and children: one editor over an outline, with
ordering derived rather than stored. It is a place you visit deliberately,
not persistent chrome. A note also has a surface of its own — the body,
with the node's children under it — which is what `enter` on a row opens
and where a long note is actually read.

**Why:** the outline is right for seeing where something sits and wrong for
reading a page of prose. Splitting them means neither has to compromise,
and both are the same text: editing a note on its surface and editing it on
the map are one edit on one CRDT.

## Deferred on purpose

- **Agent help with filing** — an agent suggesting the parent for a note,
  or gardening the tree on request. Manual filing first.
- **Briefings** — an LLM summary of what happened while you were away.
  Generated from the logs, so it can be added any time.

## Symptoms to watch for

- **Notes turning into a feed:** if any future feature wants the machine to
  write into a body, the first decision above is the answer: no. Machine
  text belongs in a typed field.
- **A title stored rather than derived.**
- **A note that can only be reached from the map**, which means the thing
  it is about lost its link to it.
