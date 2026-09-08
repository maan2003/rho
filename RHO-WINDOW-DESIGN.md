# `rho-window`: the editor primitives, and nothing about any source

Owner: eng-8gpr. This is the design note the user's ruling in
`GUI-CRATES-DESIGN.md` asks for — Rho is an editor the way Emacs is one — held
against what `rho-gui` has today, one primitive at a time. `rho-window` owns
focus, key contexts, the echo line, the minibuffer, transients, surfaces and
their history. Source crates add no primitives of their own.

The order is the transient buffer first, because `rho-slack` needs it for
reactions, and because it is the primitive that says most clearly what
`rho-window` is: if the transient can be built without reaching into a source,
the rest can.

## The transient buffer

### The spec

A small buffer that opens under the point, lists keys and their meanings the
way Magit does, takes exactly one key, and closes. The surface behind it is
undisturbed; back returns to it with the point where it was. It is drawn with
the editor primitives, as buffer text, not as a popup element.

### What `rho-gui` calls a transient today

`crates/rho-gui/src/transient.rs`, 2,101 lines. A `Transient` is a title and
rows of `(key, description, value, run, stay, when)`; 19 menus are defined in
the file (verdict, snooze, root, new, slack, status, input, hosts, four usage
menus, the phone's own); 14 call sites elsewhere in `rho-gui` open one, ask it
for its rows, or run an item by index. Keys match by keystroke notation,
`when` drops items whose context is missing at open rather than failing at
press, and a menu may be `counted`, where digits are a vim-style count for the
next item rather than keys of their own.

That much is the primitive, and it is a good one. Around it are five things
that are not.

1. **It draws into the bottom strip, not under the point.**
   `Transient::render` builds a `gpui` element tree — `div()`, `canvas()`,
   `PathBuilder` — and hands it to `minibuffer::bottom_strip`. The menu is a
   region of chrome at the bottom of the window that happens to hold text.
   The spec says buffer text under the point. These are not the same object
   rendered differently; one is an element and one is a buffer.
2. **Its actions are `&mut Workspace`.** `TransientRun` is
   `Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>`. Every
   item in every menu closes over the whole workspace, which is exactly the
   coupling `GUI-CRATES-DESIGN.md` says must appear in no crate. A transient
   cannot move to `rho-window` with this type; a source crate cannot supply an
   item to it either, because it would have to name `Workspace` to do so.
3. **It holds source data and draws it.** `Transient` carries
   `quota_usage`, `active_auth_namespaces`, `global_usage`, `agent_cost_usage`
   and `usage_days`, and lines 935–1725 — 791 of the file's 2,101 — are the
   charts those fields draw: pchip interpolation, percentile histograms, log
   scales, grid painting, per-model colours. This is a visualization of agent
   quota and cost that lives in the menu type because the menu is where it is
   shown. None of it is a window primitive.
4. **It stays open.** `stay` is magit's do-stay, so a toggle keeps the menu up
   and several toggles chain. The spec says exactly one key, then closed. This
   is a real disagreement about the primitive, not an implementation detail,
   and it is the one point where today's behaviour may be the better one — see
   the open question below.
5. **The phone has a second way in.** `phone_rows` and `action_at` let
   `workspace_phone.rs` render the same menu as a touch list and run an item by
   index. The primitive needs a way to be presented by something other than
   the key path, but by-index into a private `Vec` is not it.

### Verdict: replace the primitive, keep its shape

The data shape survives: a title, rows of key and meaning, an optional value
for infixes, applicability at open, and counts. Everything about how it reaches
the screen and what an item is allowed to touch is replaced.

Concretely, in `rho-window`:

- A transient is drawn at the bottom edge of the window, over the surface,
  in the editor's own text style. (Superseded: this said "a buffer, opened
  under the point", and it was drawn as a block in the surface's buffer. The
  user ruled that wrong — Magit's transient sits at the bottom of the frame
  and the point does not move — so the buffer is no longer touched at all.
  It is still not a strip: it is pinned over the window rather than added to
  the column, so nothing above it reflows when it opens.)
- An item's action is a value, not a closure over the window. The crate that
  supplies the menu says what it wants done in its own vocabulary; the window
  hands that value back to it when the key is pressed. `rho-window` names no
  source type and holds no `&mut Workspace`.
- The usage charts leave with their data. They are `rho-agents`' facts about
  quota and cost, summarised there and painted by `rho-gui`'s `usage`; a menu
  that shows one asks for a rendered thing rather than carrying the series.
  (This line said `rho-visualizations` until the charts moved. That crate is
  the daemon's opaque SVG blob store — record and get by sha256, no gpui, no
  idea what a chart is — so it was the wrong name for painting a live series.)
- One presentation path, not two. The phone renders the same buffer; there is
  no second by-index API.

### The open question, for the user

Toggles. The spec says one key and closed, which is right for a verdict and for
a reaction. Today's `stay` exists so several toggles chain without reopening —
the usage menus and the input menu use it. Emacs' own transients do both: a
suffix exits, an infix stays. I intend to keep exactly one key as the rule and
let an item declare itself an infix, which is the same distinction Magit draws
and the reason it is not a modal — but this is a feel question and the user
owns it. I will not build the chaining case until it is answered.

### What eng-bgkw needs

To be filled in from their change 2 landing note, forwarded by eng-en1p.
Reactions are the known case: a transient over a message, one key per emoji,
the point staying on the message it reacted to. Anything else they need goes
here before the cut, not after.

### Built

The primitive is in `rho_window::transient`: the data shape kept, the actions
values of the caller's type, the drawing a measured block under the point, one
presentation path, applicability at open, and one key then closed. `Kind::Infix`
is there for the open question above and unused until it is answered.

The wiring is moving a batch at a time. The verdicts went first; the root menu
and the three menus only it reaches — slack, hosts, projects — are the second,
which is the batch that turned the verdict menu's private plumbing into the
window's one way of showing a menu. Two things came out of doing the root menu
rather than another leaf.

An item that names a menu (`MenuId`) rather than opening one is what lets a
menu half on the buffer and half on the strip work at all: the root menu says
"hosts" and the workspace decides where hosts is drawn, which is the only
reason the seventeen can move in batches instead of one landing. It is also
the honest shape afterwards — a menu should not know how another menu is
presented.

And back has to be a stack, not a parent. The verdicts were one deep, so a
single parent was enough; `space s u` is three, and an escape that goes out
from the third step rather than back to the second is the thing the design
says never happens. What is kept is what escape retraces, so the whole way
back is there, not one step of it.

The third batch is the draft's two — `new` and `input` — and with them the
phone, which is where the primitive's fifth complaint about the old
transient is answered. The phone draws the menu as a sheet: a thumb needs a
target, not a row, so the sheet draws the menu's items as targets and the
surface behind it is left alone. What makes it one presentation path rather
than two is that it is the same `Transient<A>` read through `items()`, and a
tap runs the item a key would have run — `action_at` and `phone_rows` are
gone for every menu that has moved. A menu is open either way, and only its
drawing differs. (The desk drew a block in the buffer until the user's
ruling; now neither does, and `MenuBuffer` says nothing about where the menu
is drawn.)

What is not done: nothing of the menus. The five usage menus were the last
readers of the bottom strip and of `phone_rows`; they are now eight items of
one `Menu` over `Command::Usage`, the charts are a screen of their own
(`rho-gui`'s `usage`, over summaries from `rho-agents`' `usage`), and the
strip's element tree went with them. One orphan found on the way and removed: `phone_desk_menu`,
the Map screen's own sheet on the phone — cycle folds, edit notes, new — was
opened by nothing, because the phone's ☰ opens the root menu whichever root
is showing. A menu nothing opens is residue; if the map's own sheet is wanted
it comes back as a `Transient` over `MenuAction` with one line in the bottom
bar, not as a strip.

### The grid, put back

The move onto the primitive dropped Magit's layout: `render` drew one item
per row, which for the root menu's twenty-eight items is the screen from
top to bottom. The user asked for the grid back exactly, and it is back —
the element tree from before the move, ported unchanged into
`rho_window::transient::render`: columns of four, filled top to bottom and
then left to right, wrapping across the width with `gap_x_6`; the key in a
right-aligned `w_8` accent cell so the keys line up down a column; a value
in muted brackets around bold green; a bold title above, carrying the
count suffix the primitive added. The strip still pins it to the bottom,
which is the caller's business and unchanged.

The layout is answerable without a window — `Transient::columns` is the
chunking, and the two tests assert twenty-eight items as seven columns of
four and five as a column and a stub — so the shape is proven off-screen.
The painting is proven too, because the failure this reverses was a
layout that chunked correctly and still drew as one column — a thing only
a frame can say. Isolated rig `desk`, session 98, root menu open at
1280x832 logical: **six columns of four**, five of them across the strip
and the sixth wrapped onto a second line under the first, every item
present and none clipped. Twenty-four items rather than the root menu's
full twenty-eight, because `changes`, `attach` and the other conditional
items are not applicable in that state — applicability at open, working as
it should. The shot is `/tmp/rho-slack-ux/screens/root-menu.png`.

One thing the frame shows and the test cannot: the wrap has `gap_x_6`
between columns and no gap between wrapped lines, so the sixth column sits
directly under the first. That is what the layout did before the move as
well, and it is left as it was rather than changed under cover of a
restoration.

Cost: unchanged, O(items) per frame, and an item is a row on the screen.

### Owed

Not this crate's yet, and written down here so they are one list rather than
three landing notes.

- **The right prompt is the vendored editor's, not the window's.** A screen
  reaches it through `editor`, so today the window does not own every
  primitive its screens draw with.
- **A width change rewraps the whole buffer, and so does the wrap map on its
  own.** That fails the cost rule twice over. It is closed for the reader:
  the wrap map takes the reader's rows as a parameter of the width change,
  lays those out before the frame, and closes the rest in the background
  (main `38dd8d32`). The document is still rewrapped in full eventually,
  which is the cost of a width change and not of a frame. The landing walk
  now drives it: a transcript of forty settled turns, a width change, a jump
  to the top, and keystrokes beside them. On that document a width change
  touches all 273 rows and a keystroke touches one, so a rewrap is the width
  change's alone and an edit already costs the rows it edits. The rewrap is
  bounded there too: the transcript's composed window saturates at 273 rows
  however many turns are seeded, so the 262k-row figure below is the
  file-backed views' and not the transcript's.
- **The inlay map's edit-carrying sync is O(document).** Its per-row cost
  grows with the buffer rather than with the edit: 3.2 µs per row early in a
  `gg` over the 262k-row transcript and 13.3 µs per row late in the same
  drive, measured on desk session 46. That is an editor defect on an edit
  path, independent of what the composition hands it. Cut A makes it quiet
  for `gg` by handing it a screen instead of a history; it stays as history
  scrolling's cost, so it is written here rather than closed.
- **A block insert costing a rewrap of what is around it is closed.** It was
  two frames over the 8 ms budget when a twenty-six row menu opened into a
  121k-row Home on desk session 30. Re-measured after the fold trio landed,
  on desk session 41 — ten open-and-dismiss round trips on Home and nothing
  else — it is 115 frames, draw p99 5.8 ms, none over 8 ms, with
  `block_map_sync` p99 0.08 ms at one row. The insert costs the block now.
  It was desktop-only in any case: the phone draws the same menu as a sheet
  and inserts no block, so nothing in its buffer moves.
- **The rig can tap, and this is closed.** It was here because the phone's
  menu is reached by tap and a headless seat has no pointer device, so the
  sheet was proven by test and not by picture. The driver creates a
  `zwlr_virtual_pointer_v1` for the length of a tap now, and the sheet is
  proven by picture on desk session 45: the header tap draws it, the Status
  tap runs the row, and closing is byte-identical to the frame before the
  first tap.

### Elision, and what it must cost

Two things in a transcript are hidden rather than shown, and they are the
same act at two scales. A settled turn is elided down to its last rows, with
a chip in place of the rest that opens and closes again. Markup is
concealed: the `**` around a word is text the reader is never meant to read.
Both are folds below the wrap map, which is what makes them cheap to draw —
a hidden row leaves the wrap's input and the block map's entirely, so it
costs nothing to lay out and nothing to compose. The agent transcript is not
the only reader of this: the Slack conversation surface is a client of the
same pipeline, so what is said here about markdown, concealment and the
gutter bar holds for a Slack message too.

What they are not is byte arithmetic. The fold map keeps its ranges in inlay
offsets, so every elision and every concealment is a byte range in a
coordinate the inlay map moves under it. That is where the cost and the
faults both come from: an edit landing near a fold has to be widened to the
fold, in bytes, over the inlays on either side, in the old snapshot and the
new one at once — and the widening loops are where an output edit stops
describing a range of any document that exists. The accounting has caught
three of those on rho's own streaming path, each short by a byte or two, and
they were reachable by a reader typing into a transcript whose markdown was
already concealed.

In rho's terms neither hiding is a byte question. A turn hides because the
model says it is settled; a delimiter hides because the parse says it is
markup. Both facts are properties of a range that already carries anchors,
and an anchor survives an edit without anyone subtracting offsets. So the
cut is to say the range once, in the buffer's own coordinates, and let the
fold map hold what it is given rather than recompute where it moved to: the
end side rewritten in buffer offsets, the step over a fold rewritten with
it, and `text()` sealed so no path builds a fold range out of a rendered
string.

The cost it must hold to is the rule the rest of the window holds to, and
one clause more. Per event, O(rows the event touches) + O(log n), and never
O(elisions the document carries): a keystroke inside one turn must not pay
for the four hundred settled turns above it, and settling a turn must cost
that turn. Per frame, O(rows drawn), which folds already give.

The counter that proves it is the walk's own. Every step of the gate prints
`walk=` with a count per stage — the leaf items each stage's cursors crossed
— and `fold:` is this map's line in it. Today a keystroke on a 273-row
transcript reads `fold:4`, and composing a fresh chunk of history reads
`fold:4` as well, held flat while the composed window climbs from 299 rows
to 635. That flatness is the bar: a stage of the cut that makes `fold:`
track `total_rows`, or track the number of ranges the document hides, has
failed regardless of how the wall clock reads.

What the gate's document does not yet carry is elided turns. Its folds are
concealment — the drive's tool output is concealed before the tab map ever
counts it — so `fold:` reads markup today and not settled history, and the
first stage of the cut is to seed turns that are elided and see what the
counter says then. A number that comes to hand is not the number of the
thing until the thing is in the run. Correctness has a counter of its own
beside it, the fold map's accounting: empty, or naming the edit that
stopped describing a range. Both are read on every stage.

The cut is finished, and the counter says the clause holds. What the gate's
five paging steps show is a transient, not a slope: lengthened to fifteen,
the elided run's `fold:` rises to 184, settles at 137 and stays there —
`fold:137` at 515 rows, at 707, at 899, while `wrap:359` and `block:113`
sit flat beside it. The 66 at 232 rows and 136 at 473 that the shorter
window shows are the climb up to that plateau. The level is the elisions
the composing chunk crosses, 137 leaf items for a 48-row chunk of elided
history against 4 for the same rows unelided, and it does not follow the
document.

Because a plateau in one drive is not the property, the clause has a test of
its own: folds spread over the same rows in both cases so only their number
differs, an edit at the foot below every one of them, counting the fold
map's leaf items rather than milliseconds — 36 walked under 32 folds and 32
under 256, and 45 against 44 at 125 and 1000. Eight times the folds, the
same walk.

The clock is no use for this and it is worth saying why, because it reads
like the fault. Timed rather than counted, that edit costs 0.136s under 250
folds and 1.200s under 2000 — a clean 8.8× — but only because that version
grew the document along with the fold count. With the document held fixed
the clock inverts, 3.22s at 250 folds against 1.17s at 2000, since folding
two thousand ranges takes four thousand rows out of the layout. The wall
clock there is measuring the rows a fold hides. The cost rule is written in
counts, and the count is what the test asserts.

`block:` is flat over the same climb, 28 at 233 rows and 28 at 469, and it
is flat because it counts the walk and only the walk: rho's elisions are
folds below the wrap map, and the block map's own display elisions - which
it scans in full twice per sync, resolving two anchors through four maps
each time - have no caller outside tests, in rho or in zed, so that scan
never runs over anything on the reader's path.

### How it will be proven

On the QA rig, on the user's snapshot, with the handbook's Emacs-feel checks:
the point survives back, the same key means the same thing in every buffer,
nothing needs the mouse, no modal appears. Plus the cost rule, per event
O(touched) + O(log n) and per frame O(drawn), measured on the snapshot and
written into the landing note.

Read that way, the two largest numbers the gate prints on the elided run are
one event and not two. The largest count on a step's line, `multibuffer:216`
against 77 for the identical 64-byte chunk later on, and the largest draw in
the whole gate, 3537 us against about 2000 us for the steps after it, both
fall on the step after the window is composed: fifty-eight of the window's
buffers report a parse landing in a single sync, every excerpt is re-created
against its new snapshot, and the frame that follows carries 2767 primitives
across 161 owners where a settled one carries 1075 across 148. It is the
compose settling, once, and not a walk - the same chunk's own syncs cost a
dozen or two items each, at 59 excerpts and at 119 alike. What this asks of
a reading of the gate is that the biggest number on a line be attributed
before it is cut at: a count that follows neither the rows touched nor the
document is usually work arriving, not work repeated.

## The prompt, and what it can do per keystroke

The minibuffer is a completing read: a prompt, an input line, and candidate
rows beneath it. It has always had two callbacks, and until now only one of
them could act.

`CandidateSource` is `Fn(&Workspace, &str, &App) -> Vec<Candidate>`. It runs
on open and after every edit, and it answers one question — *what could this
input become* — with a list. It takes the workspace by shared reference on
purpose: recomputing what a reader might mean must not change what they are
looking at, and a completion that could act would be a keystroke with a side
effect the reader did not ask for.

`SubmitHandler` is `Fn(&mut Workspace, String, &mut Window, &mut Context<..>)`.
It may act, and it runs once, after the prompt has closed.

Between them was a gap, found by eng-bgkw while landing Slack search and
reported rather than worked around: a prompt could offer completions per
keystroke and could not *do* anything per keystroke. An Emacs-style narrowing
read is the list itself narrowing as the reader types — that is the whole of
it — and on submit is not that. The list behind the prompt could only narrow
after the prompt was gone.

`ChangeHandler` closes it, and is deliberately shaped like `SubmitHandler`
rather than like `CandidateSource`:
`Fn(&mut Workspace, &str, &mut Window, &mut Context<..>)`. It answers the
other question — *what should the reader be looking at now* — and it may act.
It runs once per edit, after the candidates for that edit have been recomputed
and the prompt put back, so the handler sees the workspace as the reader does,
including the prompt it belongs to, which it may read, replace or close. It
never runs per frame: a redraw is not an edit.

It is optional per prompt, and a prompt that sets none pays one `None` check
per keystroke and nothing else. `open_prompt` keeps its signature and its
twenty-six callers keep theirs; `open_prompt_watching` is the one that takes a
handler. That is the shape of the widening: nothing that did not ask for the
new power is changed by it, and the two questions stay two questions — read to
suggest, act to narrow — rather than being merged into one callback that does
both and is hard to reason about at either.

What it does not do: it does not save or restore what stood before. A prompt
that narrows something and wants escape to put it back saves that itself, on
open, because only the prompt knows what "back" means for the thing it is
narrowing. The primitive's job is to say *when*, once per keystroke, exactly.

## Surfaces and history

### The spec

The user's rule, unchanged: a surface is a buffer with the point in it,
history is a stack, and back returns the point to where it was. Everything
below is an attempt to say what that means precisely enough to cut against,
and what it costs.

**The golden rule is TikTok.** In the user's words: up (`f21`, `SurfaceBack`)
moves back through history; down (`f20`, `DealOpen`) moves forward through
history if there is anything forward, and only when the reader is at the
newest entry does down deal, opening the next thing that asks for attention
and appending it. That is the whole spec of the two keys, and it is what the
workspace did before the machine existed. There is no third key: dealing and
history are the only next and previous.

### What a surface is, and what its identity is

A surface is a place the reader can be. Today it is `Surface { key, view }`
in `rho-gui`: a `SurfaceKey` that says which place it is, and a live view
entity that holds the buffer, the point, the scroll and the folds. The two
are deliberately separate — the key is stable and cheap to compare, the view
is the expensive live thing — and the split is what lets a context keep a
buffer list (`surfaces: HashMap<ContextId, Vec<Surface>>`) while its one
viewport (`Pane<Surface>`) shows one of them.

Identity is what makes a place a different place, and nothing else. It is the
agent id, the file path, the Slack source, the browser page id — never a
label, because two threads in one channel have the same label and are two
surfaces. What a surface happens to be *showing* is not identity: the usage
screen is one `SurfaceKey::Usage` and the chart on it is its own state, so
picking another chart redraws that screen instead of opening a second place
to be. That precedent is the rule, and the design keeps it: if two things
differ only in what is drawn, they are one surface with state; if a reader
can mean one and not the other, they are two keys.

The key type stays in `rho-gui`, because every variant of it names a source
crate's idea. What moves to `rho-window` is the machine that holds surfaces
and their order, generic over the key the way `Pane<S>` already is. The
window names no source type; that is the crate's whole rule.

### What the old history did, and what a new open does

The user says this worked before the machine and must work again exactly as
it did, so the old code is the specification. At `81318e26` the workspace
held `surface_history: Vec<WarmSurface>` and a `history_cursor`. `SurfaceBack`
ran `step_surface_back`, which moved the cursor back rather than popping;
`step_surface_forward` moved it forward, and `cmd_surface_forward_or_deal`
dealt when there was nothing forward. Those two were deleted by `0707dff59a6`
on 4 September, which is when down stopped meaning forward.

The named question, answered: `append_history` deduped by key, pushed at the
end, and set the cursor to the end. **It did not truncate forward.** A new
open with the cursor in the middle appended and left what was ahead of the
reader behind them, still reachable by going back. The machine restores that,
and it is a restoration rather than an invention.

The cost was the defect, not the behaviour. There were two histories that had
to agree — `Pane<S>` kept a per-context `Vec<S>` whose `show()` did
`history.retain(...)`, and the workspace kept its own — and `append_history`
scanned with `position` and `remove`d the match, so a push was O(entries)
twice over. Closing a surface scanned again; forgetting an agent scanned both.
Nothing walked either at draw time, which is the one half of the rule that
already held.

### What the history stack must cost

Per event: **O(1)** to open, **O(1)** up, **O(1)** down, **O(1)** when the
thing behind a surface dies. Per frame: **nothing** — the viewport draws the
active surface's view and never reads the list. And a rule the cost shape is
not allowed to break: **no forward step is ever lost by the machine on its
own.**

The shape that gets there is a list with a cursor, held as a slab: entries
are `{ key, surface, prev, next, order }` in a `Vec<Option<Entry>>` with a
free list, so a slot that is forgotten is used again rather than growing the
list. `live: HashMap<Key, usize>` holds each live key's slot, which is what
makes an open dedupe without a scan: opening a surface already in the list
unlinks it from where it was and relinks it at the newest end, O(1) on both
sides. Up and down follow `prev` and `next`, one hop. Forgetting by key is a
map lookup and an unlink; if the reader is standing on the forgotten entry it
is marked rather than removed, and dropped when they step off — a stale entry
is skipped once and never twice.

**Withdrawn: history is not per context.** eng-en1p's earlier ruling that
history is per context, under the Emacs rule that buffer history is per
window within a frame, is withdrawn by the user. History is one list across
contexts again, as the old one was, so back walks into the context you came
from. The user's words are the spec: back is TikTok's up, and what is behind
you is behind you whatever it belongs to. The old code did this and the user
wants it back; the per-context version made a deal into a new agent context
start a fresh list, so back had nothing behind it and did nothing, which is
what "history is broken" looked like from the desk.

Down at the newest entry is where dealing plugs in. The machine does not know
what a card is: it reports `at_newest()`, and the workspace deals and appends
the surface it opened. That is the only place the two meet.

The measured numbers, from `rho_window::history`'s tests. One entry per live
key: 1,001 opens of 1,001 distinct surfaces make a list of 1,001 entries, and
2,000 opens alternating between two surfaces make a list of **three** — no
compaction pass, because there is nothing stale to compact. A forget returns
its slot to the free list and the next open takes it. Up then down is where
the reader started, and an open with the cursor in the middle leaves the
count ahead at zero and the count behind grown by what was ahead.

### What back restores, and what it does not

The point, and with it the scroll and the folds. It restores them by
*keeping* the view rather than by replaying anything: the view entity for a
surface stays alive in the context's surface list, so its editor still holds
its own selections, scroll position and display map when the viewport comes
back to it. This is why note bodies already survive leaving and returning.
Restoring by replay — remembering a line and a column and seeking to them —
is the design this rejects, because it is a second copy of the truth that is
wrong whenever the buffer changed underneath.

There is one surface kind where keeping the view is not enough, and it is the
commonest one on the desk. A dealt note is not its own view: `open_card`
wraps a `SurfaceKey::DeskNode` around **the dashboard's own editor** and moves
the dashboard's point to the node, so the whole of what distinguishes one note
surface from another is where that one shared point is standing. Keeping the
view keeps nothing, because every note surface keeps the same view. Stepping
to such a surface out of history therefore has to put the point back the way
the deal put it there. This is not a replay of a remembered line and column —
it is the same `move_to_tree_node_when_ready(host, node_id)` call the open
makes, addressed by node rather than by position, so it is right after the
tree changed underneath. Without it back and forward changed the title bar
and left the reader on the rows they were already reading: on the rig the
frames at each stop differed only in the title row, 1,749 pixels of
4,259,840, with the buffer beneath pixel-identical.

It follows that what the stack stores is a handle, never the only copy of a
view. Going back drops the machine's entry for the surface being left, so if
that entry owned the view, leaving B and returning to it would find B fresh
with its point, scroll and folds gone — the Emacs rule broken on the ordinary
path, since leaving a buffer never kills it. It is not: every `SurfaceView`
variant is a refcounted `Entity`, and the context's buffer list
(`surfaces: HashMap<ContextId, Vec<Surface>>`) holds a clone, which
`make_surface` returns rather than building a second view. Dropping the entry
drops a handle. The one path that really destroys a view is eviction —
`release_agent` takes an unshown transcript out of the buffer list and drops
its model — and that path already has the warm store this design would
otherwise need: `warm_surface` rebuilds the transcript if back reaches it.

Worth naming while it is in view, as an item rather than a change here:
closing a surface removes it from history but leaves it in the buffer list,
so `q` is `bury-buffer` and not `kill-buffer`. That is what main did too and
nothing depends on it either way, but the two words mean different things and
the code currently says only one of them.

It does not restore a menu. A menu is open over a surface, not part of one,
and leaving closes it; coming back finds the surface as it was with nothing
over it. The alternative — history entries that carry a transient — would
make the stack hold live UI state, and the reason to say so here is that the
question is settled rather than open.

### When the thing behind a surface goes away

An agent is deleted, a conversation is gone, a file is removed under the
reader. The surface's identity is now a place that does not exist, and it
leaves history at the moment the thing does, by key, in O(1). What it must
not do is leave a hole the cursor can fall into: back moves to the nearest
surviving entry, and if none survives, to Home, which is where a cold start
lands and the one surface that is always there. Today that invariant is kept
by four separate places agreeing about how to move a cursor after a removal —
close, discard, forget an agent, forget a daemon — and each of them is
written out longhand. I have not found a case where they disagree and I am
not claiming one; the point is that the invariant is not stated anywhere, so
nothing checks it. Death becomes one call: all four hand a key to the
machine's `forget`, and the invariant is a test on the machine rather than
four places agreeing. Under the shape above there is no cursor to fix: entries
go stale and are skipped, and a dead surface is unreachable because its key
is no longer in the map.

### How it will be proven

On the rig, on the user's snapshot, first open named. Open A, note the frame;
open B; back; the frame must be byte-identical to the one taken on A —
identical is the whole claim, because a point that moved by one row and a
scroll that moved by one pixel both show up and neither shows up in an
assertion that back returned to A. A no-input control of the same span runs
beside it, because the desk moves while agents work and two frames of Home
are not equal by default. Then the same pair with the surface scrolled and a
fold closed, and the same pair after the thing behind the second surface is
deleted while it is in the stack. Plus the cost rule from the handbook, with
the per-event numbers on the snapshot and nothing walked at draw time.
