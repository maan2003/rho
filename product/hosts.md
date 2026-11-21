# hosts

A host is a machine where agents live and work: an always-on server, or the laptop itself. The window attaches to one or more hosts and shows their projects and message together. A host also carries what the user says about cards, their notes, and their drafts between the user's paired devices, each a `Device`, sealed so the host cannot read them; see `deal.Verdict`.

## bet Agents live on hosts, and the window attaches to many
instead of: one server, or everything on the laptop.
serves: Keep the work moving.
because: agents must run when the laptop is shut, and the user has more than one machine; the window reaches each host over the network, and hosts do not trust each other.
costs: a host can be away, and the user must attach each host on each device.

## bet The user's own state lives on their devices, sealed
instead of: the host keeping what the user said about each card, their notes, and their drafts.
serves: Know what wants them, and where they were.
because: a host runs code the user did not write, and an agent on it must never read what the user decided or wrote for themselves; the hosts carry bytes they cannot open.
costs: two devices agree only when both hold the user's secret phrase.

## bet The window holds the secrets
instead of: a host holding the user's browser session or ssh login.
serves: Reach anything.
because: the window is the user's own device, and an agent that needs the user's login asks for it each time, so no agent uses it without the user's word.
costs: a prompt that opens over whatever the user is doing, and a push that fails when they do not answer in a minute.

## journey A new machine
goal: the user can put their first idea to work within minutes on a new machine.
starts when: the user has a new laptop or a new host.
steps:
1. the user attaches the host, `Attach a host`
2. the user pairs the device if it is new, `Pair this device`
3. the user puts their first idea to work, `chat.Write an idea`
done when: the user's first idea is at work within minutes.
day: Turn ideas into work.

## concept Host
purpose: be a place where the user's agents run, which the window can reach from the laptop or the phone.
principle: the user attaches the server as `rho`; its projects are available by name in `window.Palette`, with addresses ending in `@rho`; when the connection drops, the top line says so in red until it is back.
sees: its name, which the user chose, as the host part of its projects' addresses; whether it is `online`, `connecting`, or `reconnecting · 12s`, with why on the echo line when it drops, `connection lost` or `cannot reach the host`; in a thread, as part of the `chat.Project` address.
has:
- many `chat.Project`
- many `agents.Agent`
does:
- attach, by opening a pairing link the host prints, which starts `rho://`, or from the palette as `hosts › attach`: from a link, ask `name this host:` and attach the host under that name; from the palette, ask for a name and the way to reach the host, on one line, and show the form the line takes
- detach, from the palette as `hosts › detach`: ask `detach host:`, listing the hosts with their state, and detach the one chosen, which `enter` takes from the list; the echo line reads `detached: rho`; its projects, agents, and their screens go from the window, and its message comes back from the host when it is attached again; drafts and unsent message stay
- status: the `hosts` menu lists each attached host with a number, such as `1 · rho · online`; picking one says its state on the echo line

A host is reached in one of three ways:
- on this machine, where being the same user on the same machine is the trust
- over the network through the user's own ssh login: each time the window connects, it logs in to the host and tells it to trust this window for that connection
- over the network, with the user's security key: the window's identity comes from the key, so the host trusts the key and not a file on the device

The hosts attached when the window last ran are attached again when it starts. The same machine attached twice under two names is attached once, under the first name.

A pairing link saves typing where the host is. It works once, to make the host trust this device; after that the device's security key is enough. Nothing attaches until the user names the host. Opening a link while the window runs hands the link to that window, and the prompt opens there.

A host on this machine shows no agent's desktop; see `agents.Desktop`.

## concept Device
purpose: make each laptop or phone the user works on hold the same verdicts, notes, and drafts as the others, without any host being able to read them.
principle: the user sets up a second laptop and types their twelve-word secret phrase once; the cards they put away on the first laptop are away here too, and their notes and drafts are here.
sees: until it is paired, the echo line saying that its verdicts stay on this device.

A device is paired once it holds the user's secret phrase; see `Pair this device`. The first device ever makes the phrase.

## flow Pair this device
runs: 1 a quarter
day: Know what wants them, and where they were.
when: the user sets up a new device, or their first one
want: have this device hold the user's secret phrase
so that: the user's verdicts, notes, and drafts are the same here as on their other devices
steps:
1. on the first device ever, when the first host answers that it holds nothing, the prompt line reads `write down your secret phrase · it pairs your other devices · enter:` with the twelve words under it; the user writes them down, and `enter` puts them away
2. on any later device, the status line reads `verdicts stay on this device until it is paired`, in muted text, once it has seen that another device wrote
3. the user opens the palette and chooses `pair this device`; the prompt `secret phrase:` completes each word; a word that is not a phrase word, a wrong count, or a phrase that opens nothing reopens the prompt with the reason and keeps the text
4. the echo line reads `paired: this device holds what your other devices said and wrote`, and the notice goes
system: makes the phrase once, seals everything with it, refuses a phrase that does not open what the other devices wrote, and keeps no word of it in its history of prompts.
user: keeping the phrase, and typing it once per device.
cases: the user never pairs a device, and it keeps its own verdicts for good; the user puts the phrase away before writing it down, and `show my secret phrase` from the palette shows it again on any device that holds it
reviewed: 2026-11-20 · laptop · reviewer · ok

## flow Attach a host
runs: 1 a month
day: Keep the work moving.
when: the user sets up a new device, or has a new host
want: have the window reach the host and show its projects and their work
so that: the user can work with the agents on it from this device
steps:
1. the user gets a pairing link on the host and opens it on the device, as `Host.attach`
   - or the user opens the palette, chooses `hosts › attach`, and types the name and the way to reach the host
2. the user answers `name this host:` with a short name
3. the user touches the security key when the desktop asks for it; the first time on a device it asks twice
4. the host's projects are available by name, with addresses ending in the host's name
   - if the host does not trust this device yet, the top line reads `disconnected · enroll this rho on the host: …` with a command and a code; the user runs that command on the host, and the window tries again within half a minute
system: remembers the host for the next start, reconnects by itself, and asks for the security key only once each time the window starts.
user: the name, the touch of the key, and, without a link, the command on the host.
cases: the phone, where typing the way to reach a host by thumb is long and a link saves it; a host on this machine, which needs no key
reviewed: 2026-11-20 · laptop · reviewer · 2 findings

## flow Know my hosts are reachable
runs: 1 a week
day: Keep the work moving.
when: something seems quiet
want: know whether a host has dropped
so that: the user does not wait on agents they cannot reach
steps:
1. the user looks at the top line; a host that is not connected shows there in red, such as `disconnected · …`, or `2 of 3 hosts not connected`
2. if the user wants more, they type `hosts` in the palette; matching rows list each host and its state
system: reconnects by itself, waiting up to ten seconds between tries, and shows how long it has been trying; says on the echo line once when a host drops, with the reason.
user: nothing, unless a host needs attaching again.
reviewed: 2026-11-20 · laptop · reviewer · 2 findings
