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

- A transient is a buffer, opened under the point on the surface beneath it,
  drawn as text through the same editor the surfaces use. Nothing about it is a
  strip, and the bottom strip stays what it is for: the echo line and the
  minibuffer.
- An item's action is a value, not a closure over the window. The crate that
  supplies the menu says what it wants done in its own vocabulary; the window
  hands that value back to it when the key is pressed. `rho-window` names no
  source type and holds no `&mut Workspace`.
- The usage charts leave with their data. They are `rho-agents`' facts about
  quota and cost drawn by `rho-visualizations`; a menu that shows one asks for
  a rendered thing rather than carrying the series.
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

What is not done: thirteen strip menus, the phone's second way in (still
`phone_rows`/`action_at` on the old strip type), and the charts, which are the
batch that needs `rho-visualizations` rather than a mechanical move.

### How it will be proven

On the QA rig, on the user's snapshot, with the handbook's Emacs-feel checks:
the point survives back, the same key means the same thing in every buffer,
nothing needs the mouse, no modal appears. Plus the cost rule, per event
O(touched) + O(log n) and per frame O(drawn), measured on the snapshot and
written into the landing note.
