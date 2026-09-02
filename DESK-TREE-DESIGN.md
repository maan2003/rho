# The Desk as a Tree

A design for the desk's underlying primitive: what the document *is* once
the machine writes most of it. This continues DESK-DESIGN.md and keeps its
rules; it changes the store, not the experience.

This is a design, not a spec. It records the *why* behind each decision.
The engineer building it decides the mechanics, guided by these whys. Rho
has exactly one user, so nothing here needs to survive contact with anyone
else's habits.

## The problem

The desk began as a text file because text is the most freeform thing a
human can own. That was right when the human wrote every line. It is wrong
now, because the machine writes most of the desk:

- Agent rows, page rows, and every verdict (`:todo:`, `:deadline:`,
  `:defer:`, done, discarded) are machine edits into a human file. Each one
  is a CRDT splice, an undo entry, and a line the human did not type.
- Structure is *parsed* out of text: headings by stars, marks by property
  lines, agent bindings by trailing tags. Every feature that needs structure
  re-derives it from characters and hopes the parse agrees with the last one.
- Identity is a byte offset. A heading is "host + offset of its line", so an
  edit above it changes who it is. Skips, verdicts, folds, and the journal
  all key on something that moves.
- The desk's own principle, "the system never writes into the desk", is
  already false in practice and can only be kept by contortions (in-memory
  skips, tags that pretend to be text).

The design goal: keep exactly the experience the human has today, an org
outline edited with vim, and give the machine a store it can own honestly.

## Core decisions and why

### The truth is a tree of nodes; the text is a view

The desk document is a movable tree. A node has a stable id, a kind
(heading, prose, agent, page, file, draft), an owner (user or machine),
ordered children, a typed meta map (temporal marks with date, time, and
pace; bindings to agent, page, or file; tags), and its own text. Structure
changes are tree operations: create, move, reorder, delete, set meta. Text
changes are per-node text operations. The outline the human sees is a
rendering of the tree in order.

**Why:** the machine's job is structural (bind this agent here, mark this
heading done, surface that one) and the human's job is textual (write what
you think). One primitive that serves both is a tree whose nodes carry text.
With the tree as truth, a mark is a field, a binding is a field, and moving a
subtree is one operation that cannot half-succeed. Nothing needs parsing
because nothing structural is ever encoded as characters.

### Prose interleaves with items, so the desk looks the same

A node's children are an ordered mix of item nodes and prose nodes. A prose
node is a run of raw text between items, blank lines included. A heading
followed by two paragraphs, a sub-heading, and another paragraph is one
heading node with children [prose, heading, prose]. Rendered in tree order
it is byte-for-byte today's org outline.

**Why:** the rough todo list, the note under a heading, the half-thought
between two tasks: these are what made the text desk worth having, and they
must not become a form. Prose as a first-class node kind keeps freeform text
everywhere it is today, while items keep their identity around it. The
alternative, one text stream with structure markers inside it, makes every
move a text surgery and every item boundary a parse.

### Node identity is the identity

Every consumer keys on node id: the dealer's cards and skips, verdicts,
folds, the journal, the surface MRU. Agents are keyed by agent id, pages by
page id, and a heading's binding to them is a field on the node. Byte
offsets and heading titles are never identities.

**Why:** a skip that lands on the wrong heading because a line was inserted
above is not a small bug; it is the dealer lying. Stable ids are also what
make the journal analyzable: the same node across weeks is the same node.

### Ownership replaces "the system never writes"

The old rule was structural in spirit and impossible in practice. It becomes:
user nodes are only ever edited by the user; the machine creates, updates,
and removes its own nodes (agent rows, page rows, drafts), and sets meta on
a user node only through a verdict the user pressed. Machine nodes are
read-only in the editor.

**Why:** the point of the old rule was that reading the desk tells you what
*you* decided, not what some process last did. Ownership keeps that promise
precisely: your text is yours, the machine's rows are visibly the machine's,
and every field the machine sets on your node traces back to a key you
pressed and a journal line that says so.

### Structure is never parsed from text, with one deliberate exception

No document parse exists. The one recognition kept is line-local: typing
`* ` at the start of a line inside a prose run creates a heading node there,
splitting the run into prose, heading, prose. Deleting the heading between
two prose runs merges them back. Org-style keys (new sibling, new child,
promote, demote, move subtree up or down) do the same without typing stars.

**Why:** hands trained on org-mode type stars; taking that away costs more
than it saves. But the recognition is a keystroke-time convenience on one
line, reversible, and never re-run over the document. It is the opposite of
parsing: it decides once, at the moment of intent, and records a node.

### Native in the editor, not a projection beside it

The tree document is something Zed's Editor hosts directly, with the vim and
helix layers working across the whole outline: motions, search, visual mode,
text objects inside prose, folding by subtree, structure keys as editor
actions. Machine nodes render as read-only rows with today's styling:
bullets, end-of-line hints for marks, no ids anywhere.

**Why:** the current desk is a composition of excerpts spliced between
generated rows, and everything that touches structure has to reach around
the editor to do it. A third-class document is one that every new feature
has to special-case. If the desk is rho's home surface, its document must be
as native to the editor as a file is. The cost is paid once, in the vendored
editor, and every later feature (scopes, queries, bulk edits) inherits it.

### The daemon owns the document; sync is the same shape as today

The daemon stores the tree and per-node text as an operation log plus
snapshots, and syncs desktop and phone the way it syncs the text desk now.
Concurrent moves converge (cycle and delete/move rules decided by the
engineer); sibling order is a fractional index or equivalent.

**Why:** the daemon already owns the desk and the phone already edits it
offline. Changing the primitive must not change who is responsible for it.

### One migration, no backcompat

The existing org text is imported once: headings become heading nodes with
marks from property lines and bindings from tags, everything else becomes
prose nodes. The migration runs on the first start that finds a text desk,
records that it ran, and is never consulted again; it lives in one module
named as a migration so it can be deleted outright later. After it, the org
document itself is gone from the daemon, along with the parser, the tag
handling, the property lines, the text wire messages, and the offset
identities. No frozen copy is kept and no code path reads org text. The
`rho desk cat` rendering is for human eyes only; nothing parses it back.

**Why:** rho has one user and the desk is small. Carrying two truths for a
transition period is how the current state came about.

## What stays the same for the human

- The outline looks and edits exactly as now: stars, indentation, vim.
- Folding, jumping, the overview, dealing, verdict letters, marks shown as
  end-of-line hints.
- Writing anywhere. A node with no marks and no bindings is just a note.

## What changes for the machine

- Cards, skips, verdicts, folds, journal entries, and the MRU key on node id.
- Verdicts set fields; nothing is written into text.
- Agent and page rows are nodes the system owns, placed under the heading
  that binds them, updated in place, removed when archived.
- The dealer reads marks as fields; the curve library takes typed marks.
- The journal records node ids, so week-over-week analysis is possible.

## Deliberately deferred

- Scopes (a top-level node as the dealer's and the MRU's filter).
- Tags as queries: runtime groupings by field, not by hierarchy.
- Richer fields (estimates, people, links) on nodes.
- Bulk textual editing of structure (wdired-style): with the tree as truth
  it becomes a reconciler over a rendered view, and is not needed for v1.
- Multiple desk documents.

## Symptoms to watch for

- The engineer reaching for a parse of rendered text to answer a structural
  question. That is the old model leaking back.
- Machine writes appearing in user nodes' text for convenience.
- Ids surfacing in the UI.
- Prose that can no longer be written somewhere it could be written before.
- Edits in the editor that feel slower or lossier than a plain buffer.

## What done means

The org desk is imported, the text parser is gone, the dealer and journal
use node ids, machine rows are nodes, the phone and desktop sync the tree,
and a day of ordinary use in vim feels no different from the text desk.
