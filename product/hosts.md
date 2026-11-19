# hosts

A host is a machine where agents live and work: an always-on server, or the laptop itself. The window attaches to one or more hosts and shows their chat and agents together.

## concept Host
purpose: be a place where the user's agents run, which the window can reach from the laptop or the phone.
principle: the user attaches the server as `rho`; its channels appear on Home, each tagged `rho`; when the connection drops, the top line says so in red until it is back.
sees: its name, as the tag on its rooms on Home and in `window.Find`; whether it is `online`, `connecting`, `reconnecting · 12s`, or `disconnected` with the reason; when more than one is attached, the thread's host before its name at the top.
has:
- many `chat.Channel`
- many `agents.Agent`
does:
- attach, from the palette as `hosts › attach`: ask `attach host (name=local[:dir] or name=iroh:...):` and attach it under that name
- detach, from the palette as `hosts › detach`: ask `detach host:`, listing the hosts, and detach the one chosen
- status: the `hosts` menu lists each attached host with a number and its state; picking one says its state on the echo line

The hosts attached when the window last ran are attached again when it starts. A pairing link that a host prints, starting `rho://`, names, attaches, and pairs that host in one step when the window is opened with it; with the window already running, the link goes to it.

## job Know my hosts are reachable
when: something seems quiet
want: know whether a host has dropped
so that: I do not wait on agents I cannot reach
flow:
1. look at the top line; a host that is not connected shows there in red, such as `disconnected · …`, or `2 of 3 hosts not connected`
2. if I want more, open the palette and choose `hosts`; the menu lists each host and its state
system: reconnects by itself and shows how long it has been trying.
user: nothing, unless a host needs attaching again.
