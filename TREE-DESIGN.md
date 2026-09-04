# The tree: everything in rho is a node (retired 4 Sep)

Status: retired on 2026-09-04, superseded by `STORE-DESIGN.md`. Slices 1
to 4 below landed and stay landed; the store, the merge rules, the
verdict log, and sync-since-version carry over unchanged. What the store
design replaces is the rest: node kinds and machine-made reference nodes
become typed ids that are the sources' own; the `parent` copy of a
source's structure becomes a derived edge; the fields that named a source
(`agent_id`, `workspace`, `channel`, `url`, ...) become the id itself;
and where a thing is shown becomes a view rule. Slice 5 (Slack out of the
tree) was folded into `STORE-DESIGN.md` slice 2 before it was built.

## Slices, in landing order

1. Storage: cells, LWW and add-wins merge, verdict log, materializer with
   cycle-to-root and deleted-to-root, versions and sync-since, migration of
   the desk tree, the desk view kept working over it. Replaces the
   order-key and move machinery in `rho-desk` and the daemon.
2. Identity: `NodeId` through the dealer, undo, journal, and deal views;
   the inbox deleted outright (store, surface, kinds, source references,
   journal events, the word itself), the user's call on 3 Sep; captures
   as root notes, with a one-shot on first run that turns existing capture
   items into root notes and is deleted with the V1 migration after the
   upgrade; Slack inbox items are not carried, they re-derive from the
   mirror. Every agent gets a root
   `agent` node at creation, quick-spawn included, made by the daemon.
   Creation itself follows `CREATE-DESIGN.md` (3 Sep): `n` from anywhere,
   the area always asked with context first, the draft page carrying the
   agent fields; `DeskPageBind.parent` optional so the new-page flow lands
   in the tree, since the inbox it landed in is gone.
   Pulled in from slice 3 on 3 Sep so Slack never stops being dealt: a
   `thread` node is created when a ping or a reply to the user would have
   made an inbox item, through a `DeskThreadBind` request from the GUI to
   the daemon (the shape of `DeskPageBind`), and the dealer deals the node;
   Slack verdicts live in its log from then on (Slack checklist 2.10).
3. Slack: the mirror's verdict table removed; anything left that still
   deals from the mirror rather than from nodes. Landed 3 Sep: slice 2's
   cutover had already moved every decision onto thread nodes, so there
   was no verdict table left to delete and nothing dealing from the
   mirror. What this slice removed is what was left pointing the old way:
   `Model::obligations`, the dealer's entry point into the Slack store,
   dead outside its own tests; `ThreadCard::verdict_key`, renamed
   `latest`, because the newest message is a fact and the verdict keyed on
   it is the node's; and the design's claim that the mirror is the home
   for verdict state. A test pins the rule: a done thread is closed by its
   node whatever Slack still says, and is still findable so a newer
   message can rebind it.
4. Notes: multi-line `body` on the text CRDT, the note surface, "notes for
   this" from any surface, the map over the tree; `DESK-DESIGN.md` retired
   to notes-and-filing. Landed 3 Sep. The body was already multi-line in
   the store (the 4 MiB cap and the newline-free text path were there from
   slice 1); what was missing is that everything downstream read a whole
   body as a title, so `note_title` — the first line, trimmed — is now the
   one place a note is named, and paths, cards, rooms, and pickers all go
   through it. The note surface is `crates/rho-gui/src/note_view.rs`: one
   composition whose section is the node's own body buffer and whose tail
   is a generated row per child, so editing a note there and editing it on
   the map are one edit on one CRDT. `enter` on a row — on the map, in the
   finder, or on a child row of a note — opens what the row is, which for
   a note is its surface; staffing a note with an agent stays `r`.
   "Notes for this" is `ctrl-shift-n` (and `space shift-n`): the child note
   of the node the surface is about, created on the first press and
   reopened on every one after. Two keymap bugs stood in the way of a body
   ever having a second line and were fixed here: `enter` in insert mode
   fell through to the transcript prompt's `SubmitPrompt`, which ate the
   key, and on the map the mode-less `RailOpen` binding outranked both, so
   `enter` while typing opened the row instead. The map also pads the
   second and later lines of a body to its bullet's column, so a note
   reads as one block. No daemon change: the GUI already creates notes in
   its own namespace, the 4 MiB cap and the text path were there from
   slice 1, and the daemon never derived a title.

Each slice lands on its own with the tests of the slices before it green.

