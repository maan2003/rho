# hosts

A host is a machine where agents live and work: an always-on server, or the laptop itself. The window attaches to one or more hosts and shows their chat and agents together. A host also carries what the user says about cards, and their notes, between devices that hold the user's secret phrase, sealed so the host cannot read them; see `deal.Card`.

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
instead of: a host holding the user's Slack token, browser session, or ssh login.
serves: Reach anything.
because: the window is the user's own device, and an agent that needs the user's login asks for it each time, so no agent uses it without the user's word.
costs: a prompt that opens over whatever the user is doing, and a push that fails when they do not answer in a minute.

## concept Host
purpose: be a place where the user's agents run, which the window can reach from the laptop or the phone.
principle: the user attaches the server as `rho`; its channels appear on Home, each tagged `rho`; when the connection drops, the top line says so in red until it is back.
sees: its name, which the user chose, as the tag on its rooms on Home and in `window.Find`; whether it is `online`, `connecting`, `reconnecting · 12s`, or `disconnected` with the reason; when more than one is attached, the thread's host before its name at the top.
has:
- many `chat.Channel`
- many `agents.Agent`
does:
- attach, from the palette as `hosts › attach`: ask for a name and the way to reach the host, on one line, and attach it under that name; the prompt shows the form the line takes
- detach, from the palette as `hosts › detach`: ask `detach host:`, listing the hosts with their state, and detach the one chosen; its rooms, agents, and their screens go from the window
- status: the `hosts` menu lists each attached host with a number, such as `1 · rho · online`; picking one says its state on the echo line

A host is reached in one of three ways:
- on this machine, where being the same user on the same machine is the trust
- over the network through the user's own ssh login: each time the window connects, it logs in to the host and tells it to trust this window for that connection
- over the network, with the user's security key: the window's identity comes from the key, so the host trusts the key and not a file on the device

The hosts attached when the window last ran are attached again when it starts. The same machine attached twice under two names is attached once, under the first name.

A host on this machine shows no agent's desktop; see `agents.Desktop`.

## concept Pairing link
purpose: let a new device reach a host with one link, without the user typing where the host is.
principle: the host prints a pairing link that starts `rho://`; they open it on the phone; the window asks `name this host:`; they type `rho`, and the host is attached and trusts this device from then on.
sees: a link that starts `rho://`, as the host prints it; then the prompt `name this host:`.

The link works once to make the host trust this device. After that the device's security key is enough. Nothing attaches until the user names the host. Opening a link while the window runs hands the link to that window, and the prompt opens there.

## job Pair this device
status: the window has no way to take the phrase; each device keeps its own verdicts, notes, and drafts.
when: I set up a new device, or my first one
want: have this device hold my secret phrase
so that: my verdicts, notes, and drafts are the same here as on my other devices
flow:
1. on the first device ever, the window makes the phrase and shows it once; I write it down
2. on any later device, the window says that my verdicts stay on this device until it is paired
3. I type the twelve words once
4. the device has everything my other devices have said and written
system: makes the phrase once, seals everything with it, and never shows it again.
user: keeping the phrase, and typing it once per device.
cases: I never pair a device, and it keeps its own verdicts for good

## job Attach a host
when: I set up a new device, or I have a new host
want: have the window reach the host and show its threads and agents
so that: I can work with the agents on it from this device
flow:
1. on the host, get a pairing link, and open it on the device
   - or open the palette, choose `hosts › attach`, and type the name and the way to reach the host
2. answer `name this host:` with a short name
3. touch the security key when the desktop asks for it; the first time on a device it asks twice
4. the host's rooms appear on Home, tagged with its name
   - if the host does not trust this device yet, the top line reads `disconnected · enroll this rho on the host: …` with a command and a code; run that command on the host, and the window tries again within half a minute
system: remembers the host for the next start, reconnects by itself, and asks for the security key only once each time the window starts.
user: the name, the touch of the key, and, without a link, the command on the host.
cases: the phone, where typing the way to reach a host by thumb is long and a link saves it; a host on this machine, which needs no key

## job Know my hosts are reachable
when: something seems quiet
want: know whether a host has dropped
so that: I do not wait on agents I cannot reach
flow:
1. look at the top line; a host that is not connected shows there in red, such as `disconnected · …`, or `2 of 3 hosts not connected`
2. if I want more, open the palette and choose `hosts`; the menu lists each host and its state
system: reconnects by itself, waiting up to ten seconds between tries, and shows how long it has been trying; says on the echo line once when a host drops, with the reason.
user: nothing, unless a host needs attaching again.
