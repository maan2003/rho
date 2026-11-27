# hosts

A host is a machine where agents live and work: an always-on server, or the laptop itself. The window attaches to one or more hosts and shows their projects and message together. A host also carries what the user says about cards, their notes, and their bookmarks between the user's paired devices, each a `Device`, sealed so the host cannot read them; see `deal.Verdict`.

## bet Agents live on hosts, and the window attaches to many
instead of: one server, or everything on the laptop.
serves: Keep the work moving.
because: agents must run when the laptop is shut, and the user has more than one machine; the window reaches each host over the network, and hosts do not trust each other.
costs: a host can be away, and the user must attach each host on each device.

## bet The user's own state lives on their devices, sealed
instead of: the host keeping what the user said about each card, their notes, and their bookmarks.
serves: Know what wants them, and where they were.
because: a host runs code the user did not write, and an agent on it must never read what the user decided or wrote for themselves; the hosts carry bytes they cannot open.
costs: two devices agree only when both hold the user's secret phrase.

## bet The window holds the secrets
instead of: a host holding the user's browser session or ssh login.
serves: Reach anything.
because: the window is the user's own device, and an agent that needs the user's ssh login (or pushes to `main` / `master`) parks asynchronously at the top of its `chat.Thread` until the user explicitly approves it (`space a`), so no agent uses it without the user's word. Scoped platform tokens (such as `GITHUB_TOKEN` on `octo`, which blocks `main`/`master` and tags) may live on the host for feature-branch automation.
costs: protected pushes wait until the user approves them on a device that holds their ssh login.

## journey A new machine
goal: the user can put their first idea to work within minutes on a new machine.
starts when: the user opens rho for the first time on a new laptop, or sets up a new host.
steps:
1. on first launch of a new device, rho asks once whether the user is new (generate a 12-word phrase) or existing (enter their 12-word phrase), `Pair this device`
2. the user attaches the host, `Attach a host`
3. the user puts their first idea to work, `chat.Write an idea`
done when: the user's first idea is at work within minutes, with verdicts/notes/bookmarks synced.
day: Turn ideas into work.

## journey The window wakes
goal: the user wakes their laptop in the morning or after offline travel and picks up cleanly with zero false alarms and nothing lost.
starts when: the laptop wakes from sleep or regains network after time away.
steps:
1. after more than ten minutes away, `desk.Desk` is on screen (`desk.a cold return opens the desk`) showing locally cached waiting cards (counted from cache — never flashing a false `0 wait`) while `lab connecting` or `lab away` stays calmly in `quiet` in the Top-Right Global Status Bar — never red ticking counters across screens
2. queued offline ideas and replies flush cleanly to hosts as they reconnect (any refused reply returns its card; any refused offline idea surfaces locally as a `not sent` card with `write idea again`)
3. chime pressure counts awake unseen time so overnight sleep does not blast chimes on wake
4. any host that actually needs the user's hand (`needs you` — security key touch or enrollment) surfaces clearly; the user clears what waits, `deal.Clear what waits`
done when: hosts are reconnected or calmly noted as `away`, queued sends are delivered, and the user is dealing cleanly.
day: Know what wants them, and where they were.

## concept Host
purpose: be a place where the user's agents run, which the window can reach from the laptop or the phone.
principle: the user attaches the server as `rho`; its projects are available by name in `window.Places`, with `@rho` added only to a name another attached host also has; when the host is away, Desk notes `rho away` calmly while locally cached threads and waiting cards remain readable and answerable.
sees: its name, which the user chose, as the host part of a project address when two attached hosts share the project name; one unified state: `online`, `connecting`, `away` (normal everyday offline/reconnecting state — calm `quiet` tone with no ticking red counter on every screen, turning red only when the current screen or an attempted action needs that host), or `needs you` (waiting for a security key touch, device enrollment, or refused trust); in a thread, as part of the `chat.Project` address.
has:
- many `chat.Project`
- many `agents.Agent`
does:
- attach host, by opening a pairing link the host prints (which starts `rho://`), or from `window.Commands`: from a link, ask `name this host:` and attach the host under that name; from `window.Commands`, ask for a name and the way to reach the host, on one line, and show the form the line takes
- detach host, from `window.Commands`: ask `detach host:`, listing the hosts with their state, and detach the one chosen, which `enter` takes from the list; the echo line reads `detached: rho`; its projects, agents, and their screens go from the window, and its message comes back from the host when it is attached again; drafts and unsent message stay
- list hosts, from `window.Commands`: lists each attached host with its state (`rho · online`, `lab · away · connection lost`, `lab · needs you · enroll`); picking an `away` or `needs you` host copies its enrollment command or triggers reconnect
- list gccp agents / approve gccp agent / remove gccp agent, from `window.Commands` anywhere: list, approve, or remove external-harness (`GCCP`) agents on an attached host (prompting for host when more than one is attached)

## rule each device caches attached host state
when a host is attached, then the device keeps a local copy of that host's threads, waiting cards, and known runtimes (the host remains the source of truth for writes). While the host is `away`, its waiting cards stay visible and answerable on `desk.Desk` (`done` works locally on the device; replies and ideas queue until reconnect), and its threads show `<host> away` instead of `working`.

A host is reached in one of three ways:
- on this machine, where being the same user on the same machine is the trust
- over the network through the user's own ssh login: each time the window connects, it logs in to the host and tells it to trust this window for that connection
- over the network, with the user's security key: the window's identity comes from the key, so the host trusts the key and not a file on the device

The hosts attached when the window last ran are attached again when it starts. The same machine attached twice under two names is attached once, under the first name.

A pairing link saves typing where the host is. It works once, to make the host trust this device; after that the device's security key is enough. Nothing attaches until the user names the host. Opening a link while the window runs hands the link to that window, and the prompt opens there.

A host on this machine shows no agent's desktop; see `agents.Desktop`.

## concept Device
purpose: make each laptop or phone the user works on hold the same verdicts, notes, and bookmarks as the others, without any host being able to read them.
principle: on first launch on a second laptop, the user chooses existing user and types their twelve-word secret phrase once; the cards they put away on the first laptop are away here too, and their notes and bookmarks are here. If a device is lost, they revoke it from another device.
sees: until it is paired, the status line saying that its verdicts stay on this device.
does:
- list devices, from `window.Commands`: list devices trusted by attached hosts
- revoke device, from `window.Commands`: revoke a lost or retired device so attached hosts stop trusting its key and re-seal under a fresh phrase if needed

A device is paired once it holds the user's secret phrase; see `Pair this device`. Every attached host mirrors the sealed ledger so no single host is special.

## flow Pair this device
runs: 1 a quarter
day: Know what wants them, and where they were.
when: the user launches rho for the first time on a new device, or pairs an unpaired device later
want: explicitly establish whether this is a new identity or an existing one holding their secret phrase — without rho ever guessing from an empty host
so that: the user's verdicts, notes, and bookmarks stay in sync across devices and never accidentally split into two phrases
steps:
1. on **first launch of the GUI** on a new device (before any host is attached), rho asks once: existing user (`secret phrase:` with word completion) or new user (empty / generate a fresh twelve-word phrase to write down), with `escape` as the explicit choice to stay unpaired on this device only
2. on an unpaired device, the status line reads `verdicts stay on this device until it is paired`, in muted text; the user can pair later via `pair device` in `window.Commands` (which merges any local verdicts/notes made before pairing into the sealed ledger)
3. a word that is not a BIP39 phrase word, a wrong count, a checksum mismatch, or a phrase that opens nothing on attached hosts reopens the prompt with the reason and keeps the text; if entered on first launch before any host is attached, the device keeps the phrase unverified (writing no sealed records) until the first host ledger answers so a typo never splits the ledger
4. the echo line reads `paired: this device holds what your other devices said and wrote`, and the notice goes
system: asks explicitly on first GUI launch instead of guessing from an empty host or interrupting an open idea draft; mirrors the sealed ledger across every attached host; keeps no word of the phrase in prompt history.
user: choosing new vs existing user, keeping the phrase, and typing it once per device.
cases: a new laptop whose first attached host is also brand-new (never invents a second phrase by mistake); a user who stays unpaired and pairs later (pre-pairing verdicts merge); `show secret phrase` from `window.Commands` on any paired device; revoking a lost laptop via `revoke device`.
rejected:
- guessing "first device ever" when the first attached host holds nothing and popping an unsolicited phrase prompt · 2026-11-26 · split the user's ledger when attaching a brand-new host on a second laptop, and stole keypresses mid-idea

## flow Attach a host
runs: 1 a month
day: Keep the work moving.
when: the user sets up a new device, or has a new host
want: have the window reach the host and show its projects and their work
so that: the user can work with the agents on it from this device
steps:
1. the user gets a pairing link on the host and opens it on the device, as `Host.attach host`
   - or the user opens `window.Commands`, chooses `attach host`, and types the name and the way to reach the host
2. the user answers `name this host:` with a short name
3. the user touches the security key when the desktop asks for it; the first time on a device it asks twice
4. the host's projects are available by name in `window.Places`
   - if the host does not trust this device yet, the host enters `needs you` state with the enrollment command and code; the user runs that command on the host, and the window connects within half a minute
system: remembers the host for the next start, reconnects quietly by itself, and asks for the security key only once each time the window starts.
user: the name, the touch of the key, and, without a link, the command on the host.
cases: the phone, where typing the way to reach a host by thumb is long and a link saves it; a host on this machine, which needs no key

## flow Know my hosts are reachable
runs: 1 a week
day: Keep the work moving.
when: the user wants details on an `away` host or sees a host in `needs you` state
want: check each host's status and reconnect or enroll if needed
so that: the user stays aware of host reachability without noisy red alarms for normal offline hosts
steps:
1. any `away` or `connecting` host appears calmly in `quiet` in the Top-Right Global Status Bar (never repeated inside Desk); any host in `needs you` state (key touch / enroll) or an `away` host needed by the current screen shows in red in the Top-Right Global Status Bar
2. for full details and reasons, the user runs `list hosts` in `window.Commands`; each row shows host name, state, and drop reason, and picking it reconnects or copies its enrollment command
system: reconnects quietly by itself in the background (waiting up to ten seconds between tries) and keeps the latest drop reason on the host's `list hosts` row.
user: nothing for an `away` host; key touch or enrollment only for `needs you`.
