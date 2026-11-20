# rho2: the final state

Status: the design as discussed, with "As built" at the end saying where
the code that followed it on 2026-11-15 differs. The records that must
hold are Linked Specs beside the code (`agent-host/specs`,
`crates/*/specs`).

## What stays

- The agent harness: `rho-agent`, `rho-notebook`, the Python tools, the
  workset runner. One worker process per workset, Python is how agents
  reach everything.
- The chat log: one append-only log per host, dense from 1, everything
  named by the seq of the event that made it. Delivery by membership and
  mentions, mention kinds `ask`, `result`, `fyi`. The outbox keeps the
  user's messages under a client id until a host logs them. (The shape
  inside the log changes: see "Chat".)
- The agent creation protocol: post in the right topic first, then
  `CreateWorkset` or `Eval` with code that names the message as
  `origin`. The policy joins the agent to the topic and hands it the
  message first.
- The ledger: each device owns its own sealed log, hosts store and relay
  bytes they cannot read, devices merge by union ordered by
  `(at, device)`. Every device holds the same twelve-word secret. No
  host orders anything, and a device may sync through every host it is
  attached to.

## Entities

- **Peer.** Any rho process with an iroh identity: a GUI instance or a
  host. Trust is pairwise. A paired GUI can do anything on its host.
  Hosts do not trust each other.
- **Host.** Runs worksets and agents, the GitHub proxy (octo) and the
  Slack proxy with their secrets, its own chat log, and a ledger relay.
  Always-on hosts (hetzner) are where most agents live. A laptop may run
  a host beside the GUI, reachable only while it runs.
- **GUI.** One fat app, connected to many hosts at once. Holds a copy of
  every host's chat log it has seen, the user's own ledger log, and the
  browser. The browser runs on the GUI machine and is where the user
  signs into their own Slack account. A GUI may later offer services to
  agents (the browser, "did the user see this").
- **Systems.** Chat, agents, notes, browser, and other areas of the GUI.
  Host integrations, such as the Slack bot or GitHub proxy, need not have
  a system in the GUI.

## Chat: a forest of messages per host

- Streams and topics leave the model. A host log holds messages, and a
  message has an optional parent. Children are derived by the fold,
  never stored. The thread under `#41` is conversation `41`. A root
  message (no parent) is what the GUI shows as a stream; a reply to a
  root is what a topic was; replies nest without limit.
- `Posted { parent: Option<MessageId>, from, mentions, content }`.
- A move is a parent change: `Reparented { message, parent }`. The
  subtree follows.
- Membership is per thread and covers the subtree: `Joined { thread,
  agent }`, `Left`. An agent in a root receives the whole stream, like a
  bot in a channel. `leave` on a subthread cuts that branch. A mention
  reaches regardless. An agent started from a message joins the thread
  that message is in.
- `Named { thread, name }` is the only thread metadata. The host makes
  one bounded naming attempt per thread once it has a second message;
  existing names win. The GUI otherwise shows the first line of the
  anchor. Agents have no title.
- No `resolved`: an agent says done with a `result` mention, the user
  with `Settled` in the ledger, which hides the thread.
- Agent-to-agent messaging is not chat. `agents.message` is mail over
  the GCCP path, local or remote; the host keeps no log, the transcripts
  are the record. `DirectStarted` and `DIRECTS` go.
- The log is five events: `Posted`, `Reparented`, `Joined`, `Left`,
  `Named`. `Address` is `User | Agent`.
- Python: `chat.post(parent, text, mentions)`, `chat.join(thread)`,
  `chat.leave(thread)`, `chat.move(message, parent)`, `chat.name(thread,
  name)`, `chat.read(thread, last)`, `chat.roots()`. `#N` is a message
  and the thread under it.

### Around the model

- A host is named by its iroh endpoint key, not `machine_seed` plus a
  name. A message address is `<endpoint>/<seq>`; `#41` is the current
  host, `<host>#41` crosses hosts.
- The GUI shows one chat: every host's roots in one list, the host as a
  tag. A link to another host's thread resolves when the GUI has that
  host. No visibility rules: a log lives on a host; that host's agents
  and the GUIs paired with it read it. No federation between hosts.
- The user's message into a thread is a `Post` to that host, which logs
  it `from: User`. Offline, it waits in the outbox. Hosts never talk to
  each other, so a host that was off learns of it from the device.
- The draft page goes. Any message is what an agent starts from: one
  command `start agent`, a transient for where (`new on`, `join`,
  `here`) and the runtime, then the GUI sends the code. The compose box
  gets `send and start`.
- Chat becomes one crate: `protocol` shared with the
  host, fold/store/outbox behind `client`, surfaces behind `ui`.

## The user log

- The ledger as it is: one sealed append-only log per device, merged by
  union in `(at, device)` order, hosts relay bytes they cannot read. It
  holds facts (`Seen`, `Snooze`, `Deadline`, `Mute`) and the user's own
  messages: a note is `Wrote { note, text, created }`, a revision per
  save with the newest winning, and `Deleted`. The machine never writes
  note text. Notes no longer sync as slot blobs of their own.
- All of it sealed. Agents never see it. If an agent should know or act
  on something of yours, you post a message.

### Private messages

- A note is a private message: `note:<uuid>`, text in the log, and
  `About { node, about }` is its parent, which may be any address. A
  note with no parent is a private root, a room only your devices have;
  a note about a host message is a private reply under it, shown inline
  in the thread, dim and marked private.
- A reply may narrow who reads, never widen: a private reply under a
  public message is allowed, a public message is never under a private
  one. To make a private message public, post a copy.
- Host messages keep the host's dense ids: an agent sees `#41` on its
  host, the GUI holds `ThreadId { host, message }`. The host log has an
  orderer, so it needs no log per writer; private messages have none,
  so each device writes its own log. Private ids are minted UUIDs, not
  `device/seq`: no counter to keep, and two devices that convert the
  same old note say the same id.
- This replaces notes as a screen of their own, labels, `notes for
  this`, and piles.
- Reminders are not a new thing: a `Deadline` fact on any address (a
  thread, a note, a page). The text lives where the
  address lives; a bare reminder is a note with a deadline.
- Timers: nothing stores one. The dealer derives what is due from facts
  and the clock, and the GUI wakes at `next_change`. An agent's own
  timers stay in the harness.

## GUI

- The shell is a small crate that owns only what every screen shares:
  the window, panes and their history, the minibuffer, transients, the
  keymap, the peer connections, the ledger. It knows no system.
- One address scheme for every thing in every system (`thread`,
  `pr`, `note`, `agent`, `page`, `file`), as `NodeId` starts to
  do. The shell opens an address; the system that owns the kind builds
  the surface. No closed `SurfaceKey` enum. Cards, links and search hits
  all carry an address, so one `open` serves them all.
- A system plugs in five things: surfaces for its addresses, menu rows
  as data, card sources for the dealer, completion providers, search
  providers. The binary lists the systems.
- State lives per system in its own model entity (chat, agents,
  browser, attention). Surfaces observe their model. The shell routes
  keys and opens addresses. Nothing touches two systems at once.
- No sidebar and no fixed chrome. One surface at a time, the stack the
  phone has today, on every screen; panes may split where the editor
  allows. Home is the deal plus sections: unread threads, then the
  channels (roots across hosts, one list with a tag). The palette
  reaches every place by name, and by address when one is typed.
- A thread surface: breadcrumb up, messages with subthreads folded under
  their anchors, participants, composer at the bottom. `start agent`
  from any message, `send and start` from the composer.
- Agents have no list and no place of their own. A transcript opens from
  an agent's post or from the participants of a thread.
- The phone is the same surfaces in the same stack, in a layout module,
  not a second workspace file.
- Rooms, decided 2026-11-16: the GUI is a list of rooms and a room. A
  room is a root you can read, host or private, ordered by attention.
  Labels, piles, `Todo`, the usage screen, the page system and the
  message log screen go. Terminals, shells and desktops lose their
  address kinds and hang under the agent, reached from a room's
  participants (done: parts of the `agent:` address).
- Open: pane rules, reminders, the browser as a service.

## Decided against

- Units with per-peer visibility rules.
- A node/service split of the host.
- Read state in the chat log, or any fact readable by agents.
- Chat as device-owned logs merged with host logs, or private threads
  in the ledger: an outbox is simpler.
- Workset creation as a chat message interpreted by the host.
- A GUI that homes a chat stream: run a host there instead.

## As built

- The shell is `crates/rho-frame`. `Frame` is the window: one surface at
  a time from a history of addresses, Home, the minibuffer, the bottom
  transient, the hosts and the ledger. A system implements `System`
  (`kinds`, `open`, `streams`, `rows`, `claims`, `name`, `seen`, `rooms`,
  `find`, `said`, `marks_moved`, `mutes_itself`) and builds `Surface`s
  (`title`, `editor`, `subject`, `rows`); `rho-gui` lists the systems. A
  system reaches the frame only through `FrameLink`.
- Home is the list of rooms, since 2026-11-16: roots across hosts and
  notes about nothing, one row each with where it is and
  what waits in it. A room with a card in the dealer's hand goes first,
  in the hand's order, wearing the card's words; then the rooms with
  something unread, newest first; then the rest. The cards' preview and
  the per-system sections are gone from Home; `space` still deals. The
  message log screen went the same day: what rho says is the echo line
  and the process log.
- Address kinds: `thread:<endpoint>/<id>`, `agent:<id>` with its
  terminals, shell and desktops as parts under it (`agent:<id>/term/<n>`,
  `agent:<id>/shell`, `agent:<id>/desktop/<name>`), `note:<uuid>`,
  `usage:<chart>/<days>`, `page:<uuid>`. The forms that no one crate owns
  are Linked Specs beside their crates.
- iroh is the one transport. A host always binds its endpoint; a peer on
  the same machine reaches it over the IPC transport
  (`vendor/iroh-ipc-transport`): a Unix datagram socket as a native iroh
  path, found through the host's runtime directory (`--runtime-dir`,
  `$XDG_RUNTIME_DIR/rho` by default). A connection that arrives on it is
  trusted as the user's own; every other one pairs. The GUI attaches a
  local host as `<name>=local[:<runtime-dir>]`; the `rho` CLI reaches
  the host the same way. There is no Unix stream socket and no `--iroh`.
- A host is named by its iroh endpoint key, which both sessions' `Hello`
  carry: `ThreadId { host: EndpointId, message }`. `machine_seed` stays
  only as the domain agent ids are encoded in, which the agents session's
  `Host` frame carries.
- The host names a thread under a root once: when its second message
  arrives and nothing named it, a model reads those two messages and the
  host logs a `Named`. A name from the user or an agent (`rho.chat.name`)
  wins, before or after. Without any, the first line of its message serves.
- Keys: `space` is the verdict transient over the card in view, and
  again it is Home; `tab` is Home; `:` is the palette, every row of the
  frame, the surface and the systems by name, by the starts of its
  words, with the keys it has in the menus as a hint, and every place
  the systems `find`. Transients stay only where the list is the
  information: the verdicts, the snooze times, and where an agent starts.
- Starting an agent goes through the agents system's rows on a thread
  surface, from the palette: `start agent here` from the message under
  the point, `send and start agent` from a draft; where (`new on`,
  `here`, `join`) and the runtime are transients.
- The phone is a layout of the same frame, chosen by window width.
- Notes are `Wrote` facts in the log since 2026-11-16. The slot blobs
  they synced as before, their tables and the `Put` and `Slot` frames
  are gone (epoch 51); a device that held slot blobs said each once as
  a fact when it first ran a build of that day. A thread shows
  the notes about each of its messages under it as the user's private
  replies, `reply privately` makes one, and Home lists the notes about
  nothing as private rooms. `new note` makes one about nothing, with no
  prompt. A note still opens as its own surface.
- The verdicts are done, mute and snooze; the dealer still ranks a
  `Deadline`, though no verdict says one yet. Labels, piles and todos
  are gone from the GUI, the dealer and the fold; their facts stay
  in `rho_dealer::facts::Fact` so old logs decode. An old `Piled` reads
  as `Settled` said then, having seen nothing: it takes back the snooze
  and deadline before it. An old `Todo` reads as `Read`: it counts as
  seen and takes nothing back. The label facts (`Label`, `Unlabel`,
  `Repository`, `Host`, `Labeled`) are about no node and read as
  nothing.
- The usage screen and the page system went on 2026-11-16. The usage
  screen came back on 2026-11-17 at the user's request: the agents
  protocol's quota and usage calls (`QuotaUsage`, `QuotaHistory`,
  `GlobalUsage`, `AgentCostDistribution`), the `Auth` and `QuotaUsage`
  pushes and `SetAuthAccountEnabled`, the host's Claude quota probe
  (`rho-claude-usage`) and its quota observations, which the removal
  had left in the database: wire epoch 52. `rho claude-account usage`
  prints the same per-account windows for a timer, with the five-hour
  session window when the account's screen shows one. The page system and
  the browser crates (`rho-browser`, `rho-browser-wayland`, the
  extension) came back on 2026-11-19, revived from `a79f5090`, for
  `browser.Page`: a link in a message opens as a page through the
  frame's `open_link`, so chat never names the browser.
