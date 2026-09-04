# Creating things

Status: decided with the user on 2026-09-03. Sits on `STORE-DESIGN.md` (formerly `TREE-DESIGN.md`);
replaces the desk-only `shift-r`, staff, quick-spawn, and `browser › new
page` entry points. Rho has exactly one user.

## The problem

Creation is scattered and half of it is about to break. Agents start from
the desk with `shift-r`, from a heading with staff, or from the phone
menu; pages start from a browser submenu and land in the inbox as a
capture; a note is a desk heading. The inbox is being deleted, so a new
page has nowhere to land, and the desk complaint "classify a page before
you have looked at it" was answered by an inbox that no longer exists.

## Decisions

### `n` is the verb, from anywhere

`space n` opens the `new` transient on every surface, the phone included:
`a` agent, `p` page, `n` note. Nothing else creates.

**Why:** one key to learn, one place to extend, and the phone root menu
already has `n` for a new agent. It sits under the space leader with the
other verbs because a bare `n` in any editor is vim's next match (landed
that way with slice 2, 3 Sep).

### The area is always asked, and context is the first answer

Every flow begins with the area picker: Find in node-only mode (plus a
`root` row), whose first row is the node in context, so Enter alone puts
the new thing where the cursor already is. Context is the row under the
cursor on the desk or Home, the node behind the current surface (an agent,
a page, a Slack thread), else root. Typing narrows to any node.

**Why:** the user's call, 3 Sep. Every new thing has a parent the user
chose, so there is no unfiled pile and nothing to guard with a dealer
curve. Nothing new is dealt until the user marks it (`t`, `s`, a
deadline); the tree is the only landing place. The picker is the same
Find surface, so there is no second search to learn.

### Workdir and host inherit from the area

First hit wins: the area node's own file, the nearest ancestor with one,
the agent that owns the area (or the area itself when it is an agent
node), the host's only workdir, else `<choose>`. Host follows the workdir.
The same rule serves the GUI's prefill and the daemon's default when an
agent spawns an agent (the child node sits under the spawner's node).

**Why:** staff, quick-spawn, and "beside this agent" were three flows that
differed only in where the workdir came from; one inheritance rule makes
them one flow with a different area.

### Agent: the draft page carries the fields, there is no transient

`n a` → area → the draft page, body focused, with host, workdir, start
(`auto`), and role (`eng`) shown as its header fields, prefilled by
inheritance, editable in place as they are today (Tab between fields,
Shift-Tab cycles the start mode). Enter sends `NewAgent` with the area as
`desk_parent`. The `new agent` transient (host, project, workspace, role)
is removed.

**Why:** the composer already shows every inherited value; a transient in
front of it would repeat them. The user's call, 3 Sep.

### Page and note: one prompt each

`n p` → area → a URL prompt → the page node is created under the area
(`DeskPageBind` with that parent) and opened. `n n` → area → a note node
under the area, opened for editing (a heading today, the note surface
once slice 4 lands). No inbox item, no rail capture, no `captured` echo.

**Why:** with the parent chosen up front, creation and filing are one act,
and there is nothing left to file later.

### No omnibar creation

A URL typed into Find is a search, never a `new page` row. The user's
call, 3 Sep: Find finds.

## Protocol

`DeskPageBind.parent` becomes optional (`None` = root) to match
`NewAgent.desk_parent`; a note is an ordinary `DeskMutationApply`. Nothing
else changes on the wire.

## Retired

Slice 2 built the new flow beside the old one; the old one is gone as of
the change after Home. `shift-r`, staff (`u r` on a heading), quick-spawn
with a universal argument, and the `new agent` transient are unbound and
deleted, and with them the universal argument itself, which had no other
consumer. `browser › new page` had already gone with the rail. `n a` now
opens the draft page directly: the area sets the parent and the inherited
workdir, and the page's own fields are the only configuration.

## Deliberately deferred

- Terminals as a `new` kind.
- Creating from the phone with a full draft page; the phone keeps the
  short composer it has.

## What done means

From any surface, `n a`, Enter, a sentence, Enter starts an agent under
the thing in view with the right workdir; `n p`, Enter, a URL opens a page
filed there; `n n` makes a note there; the old entry points are gone; a
screenshot of the area picker with the context row first, and of the draft
page with inherited fields, from fakes.
