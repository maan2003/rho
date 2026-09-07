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
single parent was enough; `space a s` is three, and an escape that goes out
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
  which is the cost of a width change and not of a frame.
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

### How it will be proven

On the QA rig, on the user's snapshot, with the handbook's Emacs-feel checks:
the point survives back, the same key means the same thing in every buffer,
nothing needs the mouse, no modal appears. Plus the cost rule, per event
O(touched) + O(log n) and per frame O(drawn), measured on the snapshot and
written into the landing note.

## Surfaces and history

### The spec

The user's rule, unchanged: a surface is a buffer with the point in it,
history is a stack, and back returns the point to where it was. Everything
below is an attempt to say what that means precisely enough to cut against,
and what it costs.

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

### What history is today, and what is wrong with it

There are two histories, which is the first defect.

`Pane<S>` keeps a per-context `Vec<S>` and `back()` pops it. `show()` first
does `history.retain(|c| *c != previous)`, so a push walks the whole stack.

`Workspace` keeps a second one — `surface_history: Vec<WarmSurface>` with a
`history_cursor` — and this is the one the keys actually reach:
`SurfaceBack` runs `step_surface_back`, which moves the cursor rather than
popping. `append_history` scans it with `position`, `remove`s the match, and
pushes. Closing a surface scans it again; forgetting an agent scans both.

So a push is O(entries) twice over, in two places that must agree about what
happened, and the journal records both. Nothing walks either at draw time,
which is the one half of the rule that already holds.

### What the history stack must cost

Per event: **O(1)** to push, **O(1)** amortised to go back, **O(1)** when the
thing behind a surface dies. Per frame: **nothing** — the viewport draws the
active surface's view and never reads the stack.

The shape that gets there. Entries are appended and never removed from the
middle: a push is a `Vec::push`. Dedupe is by a `HashMap<Key, usize>` holding
the index of each key's latest entry, so pushing the same surface twice
leaves a stale entry behind rather than paying a scan and a memmove to
delete it. Going back pops and skips any entry whose index is not the one the
map holds for its key — each stale entry is skipped at most once, which is
what makes the amortised bound. A surface whose thing has gone is dropped by
removing its key from the map, one operation at the moment of death rather
than a scan of everything; its entries are then skipped like any other stale
one. When stale entries outnumber live ones the stack compacts, which is O(n)
against n pushes that paid for it.

Ruled by eng-en1p under the Emacs rule: **history is per context** — a
context is a task's window arrangement, which is Emacs's frame, and buffer
history is per window within a frame, never across frames; a back that
changes context moves two things at once, which breaks one key one meaning,
and entering another surface's context and returning is that switch's own
memory rather than a history entry. Named here so the user can reverse it by
name.

The measured numbers, from `rho_window::history`'s tests. At 1,000 entries a
push touches one entry and rebuilds nothing; a back touches one entry, plus
each stale or forgotten entry stepped over exactly once in the life of the
stack; a forget touches no entries at all, only the map. The rebuild is the
only O(n) event and it is rare: 1,000 distinct surfaces push with **zero**
rebuilds, and the worst case for staleness — two surfaces alternating, 2,000
pushes — rebuilds **142** times, once per fourteen pushes, leaving a stack of
twelve entries for three reachable surfaces.

### What back restores, and what it does not

The point, and with it the scroll and the folds. It restores them by
*keeping* the view rather than by replaying anything: the view entity for a
surface stays alive in the context's surface list, so its editor still holds
its own selections, scroll position and display map when the viewport comes
back to it. This is why note bodies already survive leaving and returning.
Restoring by replay — remembering a line and a column and seeking to them —
is the design this rejects, because it is a second copy of the truth that is
wrong whenever the buffer changed underneath.

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
